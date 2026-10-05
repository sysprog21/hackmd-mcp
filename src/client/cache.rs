//! The client's two short-lived caches: a workspace's note list, and the
//! account's `userPath` and teams. Both exist because resolving a
//! `hackmd.io/@owner/slug` reference needs all three before it can name a
//! note, and `HackMD` allows only 100 requests per five minutes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use super::HackmdError;
use crate::{
    dto::{NoteResponse, TeamResponse},
    models::Workspace,
};

/// What one list request produced, shared with every caller that waited on it.
pub(super) type ListOutcome = Result<Arc<[NoteResponse]>, HackmdError>;

/// The account's `userPath` and team list, kept for the note-list TTL.
/// Resolving an `@owner/slug` reference needs both before it can pick a
/// workspace, so without this every URL costs two requests before the list.
#[derive(Debug)]
pub(super) struct AccountCache {
    pub(super) ttl: Duration,
    pub(super) user_path: Mutex<Option<(tokio::time::Instant, Arc<str>)>>,
    pub(super) teams: Mutex<Option<(tokio::time::Instant, Arc<[TeamResponse]>)>>,
}

impl AccountCache {
    pub(super) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            user_path: Mutex::new(None),
            teams: Mutex::new(None),
        }
    }
}

pub(super) fn fresh<T: ?Sized>(
    slot: &Mutex<Option<(tokio::time::Instant, Arc<T>)>>,
    ttl: Duration,
) -> Option<Arc<T>> {
    let slot = lock(slot);
    slot.as_ref()
        .filter(|(stored, _)| stored.elapsed() < ttl)
        .map(|(_, value)| Arc::clone(value))
}

pub(super) fn store<T: ?Sized>(
    slot: &Mutex<Option<(tokio::time::Instant, Arc<T>)>>,
    value: Arc<T>,
) {
    *lock(slot) = Some((tokio::time::Instant::now(), value));
}

/// Locks `mutex`, poisoned or not: every critical section here leaves the
/// cache consistent, so a panic elsewhere is no reason to stop serving it.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const MAX_CACHED_WORKSPACES: usize = 32;
const MAX_CACHED_NOTE_LIST_BYTES: usize = 8 * 1024 * 1024;

/// One workspace's list, when it was fetched, and its LRU position.
#[derive(Debug)]
struct CachedNotes {
    stored: tokio::time::Instant,
    last_access: u64,
    size_bytes: usize,
    notes: Arc<[NoteResponse]>,
}

/// A short-lived copy of a workspace's note list.
///
/// `HackMD` has no note-list pagination and no conditional GET, so listing is
/// all-or-nothing and the same list backs both the list tool and every URL
/// reference resolution. A TTL of zero disables the cache, which is what the
/// tests use so their request counts stay meaningful.
#[derive(Debug)]
pub(super) struct NotesCache {
    ttl: Duration,
    max_entries: usize,
    max_bytes: usize,
    state: Mutex<NotesCacheState>,
}

#[derive(Debug, Default)]
struct NotesCacheState {
    generation: u64,
    access_clock: u64,
    total_bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
    entries: HashMap<Workspace, CachedNotes>,
    flights: HashMap<Workspace, Arc<CacheFlight>>,
}

#[derive(Debug)]
pub(super) struct CacheFlight {
    notify: tokio::sync::Notify,
    /// Set once, when the fill ends: with its outcome when waiters may use
    /// it, `None` when they must look again.
    finished: OnceLock<Option<ListOutcome>>,
}

impl CacheFlight {
    fn new() -> Self {
        Self {
            notify: tokio::sync::Notify::new(),
            finished: OnceLock::new(),
        }
    }

    /// Waits for the fill, and returns what it fetched when that is usable
    /// as is: a list no write overtook, or an error. Waiters then share the
    /// one request, even when the list is too large to cache or the fetch
    /// failed, rather than each repeating it. `None` sends the caller back
    /// to look again.
    pub(super) async fn wait(&self) -> Option<ListOutcome> {
        loop {
            let notified = self.notify.notified();
            if let Some(finished) = self.finished.get() {
                return finished.clone();
            }
            notified.await;
        }
    }

    fn finish(&self, outcome: Option<ListOutcome>) {
        let _ = self.finished.set(outcome);
        self.notify.notify_waiters();
    }
}

pub(super) enum CacheLookup {
    Hit(Arc<[NoteResponse]>),
    Wait(Arc<CacheFlight>),
    Fill {
        generation: u64,
        flight: Arc<CacheFlight>,
    },
}

impl NotesCache {
    pub(super) fn new(ttl: Duration) -> Self {
        Self::with_limits(ttl, MAX_CACHED_WORKSPACES, MAX_CACHED_NOTE_LIST_BYTES)
    }

    fn with_limits(ttl: Duration, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            ttl,
            max_entries,
            max_bytes,
            state: Mutex::new(NotesCacheState::default()),
        }
    }

    pub(super) fn begin(&self, workspace: &Workspace, bypass_cache: bool) -> CacheLookup {
        let mut state = lock(&self.state);
        if !bypass_cache && !self.ttl.is_zero() {
            state.access_clock = state.access_clock.wrapping_add(1);
            let access = state.access_clock;
            let hit = state.entries.get_mut(workspace).and_then(|cached| {
                (cached.stored.elapsed() < self.ttl).then(|| {
                    cached.last_access = access;
                    Arc::clone(&cached.notes)
                })
            });
            if let Some(notes) = hit {
                state.hits = state.hits.saturating_add(1);
                tracing::debug!(
                    cache_event = "hit",
                    hits = state.hits,
                    misses = state.misses,
                    evictions = state.evictions,
                    cached_bytes = state.total_bytes,
                    "HackMD note-list cache"
                );
                return CacheLookup::Hit(notes);
            }
            evict(&mut state, workspace, "expired");
        }
        state.misses = state.misses.saturating_add(1);
        if let Some(flight) = state.flights.get(workspace) {
            tracing::debug!(
                cache_event = "miss",
                coalesced = true,
                hits = state.hits,
                misses = state.misses,
                evictions = state.evictions,
                cached_bytes = state.total_bytes,
                "HackMD note-list cache"
            );
            return CacheLookup::Wait(Arc::clone(flight));
        }
        tracing::debug!(
            cache_event = "miss",
            coalesced = false,
            hits = state.hits,
            misses = state.misses,
            evictions = state.evictions,
            cached_bytes = state.total_bytes,
            "HackMD note-list cache"
        );
        let generation = state.generation;
        let flight = Arc::new(CacheFlight::new());
        state.flights.insert(workspace.clone(), Arc::clone(&flight));
        CacheLookup::Fill { generation, flight }
    }

    fn finish(
        &self,
        workspace: &Workspace,
        generation: u64,
        flight: &Arc<CacheFlight>,
        outcome: Option<&ListOutcome>,
    ) -> bool {
        let mut state = lock(&self.state);
        let current = state
            .flights
            .get(workspace)
            .is_some_and(|current| Arc::ptr_eq(current, flight));
        if current {
            state.flights.remove(workspace);
        }

        // A list fetched across a write is stale before it lands; an error is
        // not, and is shared either way.
        let fresh = current && state.generation == generation;
        if fresh && let Some(Ok(notes)) = outcome {
            self.store(&mut state, workspace, notes);
        }
        let shared = outcome.filter(|outcome| fresh || outcome.is_err());
        flight.finish(shared.cloned());
        fresh && outcome.is_some()
    }

    /// Replaces the workspace's entry, making room by expiry and then LRU.
    fn store(
        &self,
        state: &mut NotesCacheState,
        workspace: &Workspace,
        notes: &Arc<[NoteResponse]>,
    ) {
        let size_bytes = cached_note_bytes(workspace, notes);
        evict(state, workspace, "replacement");
        if self.ttl.is_zero() {
            return;
        }
        if self.max_entries == 0 || size_bytes > self.max_bytes {
            tracing::debug!(
                cache_event = "skip",
                reason = "byte_capacity",
                entry_bytes = size_bytes,
                max_cached_bytes = self.max_bytes,
                "HackMD note-list cache"
            );
            return;
        }
        let expired = state
            .entries
            .iter()
            .filter(|&(_key, cached)| cached.stored.elapsed() >= self.ttl)
            .map(|(key, _cached)| key.clone())
            .collect::<Vec<_>>();
        for key in expired {
            evict(state, &key, "expired");
        }
        while state.entries.len() >= self.max_entries
            || state.total_bytes.saturating_add(size_bytes) > self.max_bytes
        {
            let Some(lru) = state
                .entries
                .iter()
                .min_by_key(|(_, cached)| cached.last_access)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            evict(state, &lru, "capacity");
        }
        state.access_clock = state.access_clock.wrapping_add(1);
        state.entries.insert(
            workspace.clone(),
            CachedNotes {
                stored: tokio::time::Instant::now(),
                last_access: state.access_clock,
                size_bytes,
                notes: Arc::clone(notes),
            },
        );
        state.total_bytes = state.total_bytes.saturating_add(size_bytes);
        tracing::debug!(
            cache_event = "fill",
            cached_bytes = state.total_bytes,
            max_cached_bytes = self.max_bytes,
            "HackMD note-list cache"
        );
    }

    /// Hits, misses, and bytes held, for the benchmark that reports them.
    #[cfg(test)]
    pub(super) fn stats(&self) -> (u64, u64, usize) {
        let state = lock(&self.state);
        (state.hits, state.misses, state.total_bytes)
    }

    /// Called after any note write. Clearing every workspace rather than one is
    /// deliberate: a note can move between workspaces, and the map holds at
    /// most a handful of entries.
    pub(super) fn invalidate(&self) {
        let mut state = lock(&self.state);
        state.generation = state.generation.wrapping_add(1);
        if !state.entries.is_empty() {
            state.evictions = state
                .evictions
                .saturating_add(u64::try_from(state.entries.len()).unwrap_or(u64::MAX));
            tracing::debug!(
                cache_event = "eviction",
                reason = "invalidation",
                count = state.entries.len(),
                evictions = state.evictions,
                "HackMD note-list cache"
            );
        }
        state.entries.clear();
        state.total_bytes = 0;
    }
}

/// Drops the workspace's entry, if any, and counts and logs why.
fn evict(state: &mut NotesCacheState, workspace: &Workspace, reason: &'static str) {
    if let Some(removed) = state.entries.remove(workspace) {
        state.total_bytes = state.total_bytes.saturating_sub(removed.size_bytes);
        state.evictions = state.evictions.saturating_add(1);
        tracing::debug!(
            cache_event = "eviction",
            reason,
            evictions = state.evictions,
            cached_bytes = state.total_bytes,
            "HackMD note-list cache"
        );
    }
}

fn cached_note_bytes(workspace: &Workspace, notes: &[NoteResponse]) -> usize {
    let workspace_bytes = std::mem::size_of::<Workspace>()
        + match workspace {
            Workspace::Personal => 0,
            Workspace::Team { team_path } => team_path.capacity(),
        };
    notes.iter().fold(
        workspace_bytes.saturating_add(std::mem::size_of_val(notes)),
        |total, note| total.saturating_add(note_heap_bytes(note)),
    )
}

fn note_heap_bytes(note: &NoteResponse) -> usize {
    fn optional(value: Option<&String>) -> usize {
        value.map_or(0, String::capacity)
    }

    let strings = note.id.capacity()
        + note.title.capacity()
        + optional(note.short_id.as_ref())
        + optional(note.publish_link.as_ref())
        + optional(note.content.as_ref())
        + optional(note.description.as_ref())
        + optional(note.permalink.as_ref())
        + optional(note.user_path.as_ref())
        + optional(note.team_path.as_ref());
    let tags = note.tags.iter().fold(
        note.tags.capacity() * std::mem::size_of::<String>(),
        |sum, tag| sum.saturating_add(tag.capacity()),
    );
    let user = note.last_change_user.as_ref().map_or(0, |user| {
        user.name.capacity()
            + user.user_path.capacity()
            + user.photo.capacity()
            + optional(user.biography.as_ref())
    });
    let folders = note.folder_paths.iter().fold(
        note.folder_paths.capacity() * std::mem::size_of::<crate::dto::FolderPathResponse>(),
        |sum, folder| sum.saturating_add(folder.id.capacity()),
    );
    strings
        .saturating_add(tags)
        .saturating_add(user)
        .saturating_add(folders)
}

pub(super) struct CacheFill<'a> {
    cache: &'a NotesCache,
    workspace: Workspace,
    generation: u64,
    flight: Arc<CacheFlight>,
    completed: bool,
}

impl<'a> CacheFill<'a> {
    pub(super) fn new(
        cache: &'a NotesCache,
        workspace: Workspace,
        generation: u64,
        flight: Arc<CacheFlight>,
    ) -> Self {
        Self {
            cache,
            workspace,
            generation,
            flight,
            completed: false,
        }
    }

    /// Publishes the fill's outcome; true when no write overtook it.
    pub(super) fn complete(mut self, outcome: &ListOutcome) -> bool {
        let accepted = self.cache.finish(
            &self.workspace,
            self.generation,
            &self.flight,
            Some(outcome),
        );
        self.completed = true;
        accepted
    }
}

impl Drop for CacheFill<'_> {
    fn drop(&mut self) {
        if !self.completed {
            let _ = self
                .cache
                .finish(&self.workspace, self.generation, &self.flight, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    /// Completes a fill with `notes`; true when it was stored.
    fn fill(
        cache: &NotesCache,
        workspace: &Workspace,
        generation: u64,
        flight: &Arc<super::CacheFlight>,
        notes: &Arc<[crate::dto::NoteResponse]>,
    ) -> bool {
        cache.finish(workspace, generation, flight, Some(&Ok(Arc::clone(notes))))
    }

    use super::{CacheFill, CacheLookup, MAX_CACHED_WORKSPACES, NotesCache, cached_note_bytes};
    use crate::models::Workspace;

    #[test]
    fn invalidation_generation_rejects_an_older_in_flight_fill() {
        let cache = NotesCache::new(Duration::from_secs(60));
        let CacheLookup::Fill { generation, flight } = cache.begin(&Workspace::Personal, false)
        else {
            panic!("empty cache should start a fill");
        };
        cache.invalidate();
        let notes: Arc<[crate::dto::NoteResponse]> = Vec::new().into();

        assert!(!fill(
            &cache,
            &Workspace::Personal,
            generation,
            &flight,
            &notes
        ));
        let CacheLookup::Fill { generation, flight } = cache.begin(&Workspace::Personal, false)
        else {
            panic!("stale fill must not repopulate the cache");
        };
        let _ = cache.finish(&Workspace::Personal, generation, &flight, None);
    }

    #[tokio::test]
    async fn dropping_a_fill_leader_wakes_coalesced_waiters() {
        let cache = NotesCache::new(Duration::from_secs(60));
        let CacheLookup::Fill { generation, flight } = cache.begin(&Workspace::Personal, false)
        else {
            panic!("empty cache should start a fill");
        };
        let fill = CacheFill::new(&cache, Workspace::Personal, generation, Arc::clone(&flight));
        drop(fill);

        tokio::time::timeout(Duration::from_millis(50), flight.wait())
            .await
            .expect("abandoned fill should wake waiters");
    }

    #[test]
    fn cache_capacity_evicts_the_least_recently_used_workspace() {
        let cache = NotesCache::new(Duration::from_secs(60));
        let notes: Arc<[crate::dto::NoteResponse]> = Vec::new().into();
        for index in 0..MAX_CACHED_WORKSPACES {
            let workspace = Workspace::Team {
                team_path: format!("team-{index}"),
            };
            let CacheLookup::Fill { generation, flight } = cache.begin(&workspace, false) else {
                panic!("new workspace should miss");
            };
            assert!(fill(&cache, &workspace, generation, &flight, &notes));
        }
        let recently_used = Workspace::Team {
            team_path: "team-0".to_owned(),
        };
        assert!(matches!(
            cache.begin(&recently_used, false),
            CacheLookup::Hit(_)
        ));
        let newest = Workspace::Team {
            team_path: format!("team-{MAX_CACHED_WORKSPACES}"),
        };
        let CacheLookup::Fill { generation, flight } = cache.begin(&newest, false) else {
            panic!("new workspace should miss");
        };
        assert!(fill(&cache, &newest, generation, &flight, &notes));

        assert_eq!(
            cache
                .state
                .lock()
                .expect("cache mutex should lock")
                .entries
                .len(),
            MAX_CACHED_WORKSPACES
        );
        assert!(matches!(
            cache.begin(&recently_used, false),
            CacheLookup::Hit(_)
        ));
        let least_recently_used = Workspace::Team {
            team_path: "team-1".to_owned(),
        };
        let CacheLookup::Fill { generation, flight } = cache.begin(&least_recently_used, false)
        else {
            panic!("least-recently-used workspace should have been evicted");
        };
        let _ = cache.finish(&least_recently_used, generation, &flight, None);
    }

    #[test]
    fn cache_byte_capacity_counts_allocations_and_evicts_lru_entries() {
        let first_workspace = Workspace::Team {
            team_path: "first".to_owned(),
        };
        let second_workspace = Workspace::Team {
            team_path: "second".to_owned(),
        };
        let mut first_note: crate::dto::NoteResponse =
            serde_json::from_str(r#"{"id":"first","title":"First"}"#)
                .expect("note fixture should deserialize");
        let mut reserved_content = String::with_capacity(2_048);
        reserved_content.push('x');
        first_note.content = Some(reserved_content);
        let first: Arc<[crate::dto::NoteResponse]> = vec![first_note].into();
        let second: Arc<[crate::dto::NoteResponse]> = vec![
            serde_json::from_str(r#"{"id":"second","title":"Second"}"#)
                .expect("note fixture should deserialize"),
        ]
        .into();
        let first_bytes = cached_note_bytes(&first_workspace, &first);
        let second_bytes = cached_note_bytes(&second_workspace, &second);
        assert!(first_bytes >= 2_048, "String capacity must be accounted");
        let cache =
            NotesCache::with_limits(Duration::from_secs(60), 10, first_bytes.max(second_bytes));

        for (workspace, notes) in [(&first_workspace, &first), (&second_workspace, &second)] {
            let CacheLookup::Fill { generation, flight } = cache.begin(workspace, false) else {
                panic!("new workspace should miss");
            };
            assert!(fill(&cache, workspace, generation, &flight, notes));
        }
        assert!(matches!(
            cache.begin(&second_workspace, false),
            CacheLookup::Hit(_)
        ));

        let state = cache.state.lock().expect("cache mutex should lock");
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries.contains_key(&second_workspace));
        assert_eq!(state.total_bytes, second_bytes);
        assert_eq!(state.evictions, 1);
        assert_eq!(state.hits, 1);
        assert_eq!(state.misses, 2);
    }

    #[test]
    fn zero_or_undersized_byte_capacity_disables_storage_without_rejecting_fill() {
        let workspace = Workspace::Personal;
        let notes: Arc<[crate::dto::NoteResponse]> = vec![
            serde_json::from_str(r#"{"id":"id","title":"Title"}"#)
                .expect("note fixture should deserialize"),
        ]
        .into();
        let required = cached_note_bytes(&workspace, &notes);

        for max_bytes in [0, required - 1] {
            let cache = NotesCache::with_limits(Duration::from_secs(60), 1, max_bytes);
            let CacheLookup::Fill { generation, flight } = cache.begin(&workspace, false) else {
                panic!("empty cache should miss");
            };
            assert!(fill(&cache, &workspace, generation, &flight, &notes));
            assert!(cache.state.lock().expect("cache mutex").entries.is_empty());
            assert!(matches!(
                cache.begin(&workspace, false),
                CacheLookup::Fill { .. }
            ));
        }
    }

    #[test]
    fn oversized_refresh_removes_the_previous_cached_value() {
        let workspace = Workspace::Personal;
        let original: Arc<[crate::dto::NoteResponse]> = vec![
            serde_json::from_str(r#"{"id":"id","title":"Original"}"#)
                .expect("note fixture should deserialize"),
        ]
        .into();
        let limit = cached_note_bytes(&workspace, &original);
        let cache = NotesCache::with_limits(Duration::from_secs(60), 1, limit);
        let CacheLookup::Fill { generation, flight } = cache.begin(&workspace, false) else {
            panic!("empty cache should miss");
        };
        assert!(fill(&cache, &workspace, generation, &flight, &original));

        let mut oversized_note: crate::dto::NoteResponse =
            serde_json::from_str(r#"{"id":"id","title":"Changed"}"#)
                .expect("note fixture should deserialize");
        oversized_note.content = Some("x".repeat(limit));
        let oversized: Arc<[crate::dto::NoteResponse]> = vec![oversized_note].into();
        let CacheLookup::Fill { generation, flight } = cache.begin(&workspace, true) else {
            panic!("refresh should begin a fill");
        };
        assert!(fill(&cache, &workspace, generation, &flight, &oversized));

        let state = cache.state.lock().expect("cache mutex should lock");
        assert!(state.entries.is_empty());
        assert_eq!(state.total_bytes, 0);
        assert_eq!(state.evictions, 1);
    }

    #[tokio::test]
    async fn a_fill_prunes_expired_entries_for_other_workspaces() {
        let cache = NotesCache::new(Duration::from_millis(5));
        let notes: Arc<[crate::dto::NoteResponse]> = Vec::new().into();
        for team_path in ["old-a", "old-b"] {
            let workspace = Workspace::Team {
                team_path: team_path.to_owned(),
            };
            let CacheLookup::Fill { generation, flight } = cache.begin(&workspace, false) else {
                panic!("new workspace should miss");
            };
            assert!(fill(&cache, &workspace, generation, &flight, &notes));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let current = Workspace::Personal;
        let CacheLookup::Fill { generation, flight } = cache.begin(&current, false) else {
            panic!("new workspace should miss");
        };
        assert!(fill(&cache, &current, generation, &flight, &notes));
        let state = cache.state.lock().expect("cache mutex should lock");
        assert_eq!(state.entries.len(), 1);
        assert!(state.entries.contains_key(&Workspace::Personal));
    }
}
