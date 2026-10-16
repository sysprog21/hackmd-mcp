//! Conditional body snapshots. A cached value is never served without a GET:
//! it comes back only on a 304 to the validator stored with it, and `HackMD`'s
//! validator changes with any edit to the note, body or title (measured by
//! `tests/live-validators.rs`). So a snapshot can be older than a write and
//! still never be served after it; invalidating on a write only releases
//! memory and starts later reads afresh.
use super::HackmdError;
use crate::models::Workspace;
use reqwest::header::HeaderValue;
use std::{
    collections::HashMap,
    mem::size_of,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

type Key = (Workspace, String);
type Outcome = Result<Arc<Snapshot>, HackmdError>;

// No private note text appears in Debug output.
pub(crate) struct Snapshot {
    pub(crate) content: Arc<String>,
    digest: OnceLock<[u8; 32]>,
    pub(crate) title: String,
    pub(crate) last_changed_at: Option<i64>,
    pub(super) validator: Option<HeaderValue>,
}

impl Snapshot {
    pub(crate) fn new(
        content: String,
        title: String,
        last_changed_at: Option<i64>,
        validator: Option<HeaderValue>,
    ) -> Self {
        Self {
            content: Arc::new(content),
            digest: OnceLock::new(),
            title,
            last_changed_at,
            validator,
        }
    }

    /// The body's digest, computed on first use: a body read only to be
    /// resent never needs one.
    pub(crate) fn digest(&self) -> [u8; 32] {
        *self
            .digest
            .get_or_init(|| crate::local::offload(|| crate::hash::body_digest(&self.content)))
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("content_bytes", &self.content.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Entry {
    snapshot: Arc<Snapshot>,
    size_bytes: usize,
    inserted: tokio::time::Instant,
    access: u64,
}

#[derive(Debug, Default)]
struct State {
    clock: u64,
    bytes: usize,
    entries: HashMap<Key, Entry>,
    flights: HashMap<Key, Arc<Flight>>,
}

#[derive(Debug)]
pub(super) struct NoteSnapshotCache {
    ttl: Duration,
    max_bytes: usize,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
pub(super) struct Flight {
    notify: tokio::sync::Notify,
    outcome: OnceLock<Option<Outcome>>,
}

impl Flight {
    /// The fill's outcome, or `None` when it was cancelled before it had one.
    pub(super) async fn wait(&self) -> Option<Outcome> {
        loop {
            let notified = self.notify.notified();
            if let Some(outcome) = self.outcome.get() {
                return outcome.clone();
            }
            notified.await;
        }
    }
    fn finish(&self, outcome: Option<Outcome>) {
        let _ = self.outcome.set(outcome);
        self.notify.notify_waiters();
    }
}

pub(super) enum Lookup<'a> {
    Disabled,
    Wait(Arc<Flight>),
    Fill {
        guard: Fill<'a>,
        previous: Option<Arc<Snapshot>>,
    },
}

impl NoteSnapshotCache {
    pub(super) fn new(ttl: Duration, max_bytes: usize) -> Self {
        Self {
            ttl,
            max_bytes,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn disabled(&self) -> bool {
        self.max_bytes == 0 || self.ttl.is_zero()
    }

    /// Whether an entry whose body and stored strings (title, IDs,
    /// validator) total `bytes` could be kept, counting its fixed structures
    /// too, so a caller copies a body only when it would be. `store` still
    /// judges the exact capacities.
    pub(super) fn accepts(&self, bytes: usize) -> bool {
        !self.disabled() && bytes.saturating_add(FIXED_ENTRY_BYTES) <= self.max_bytes
    }

    pub(super) fn begin(&self, workspace: &Workspace, id: &str) -> Lookup<'_> {
        if self.disabled() {
            return Lookup::Disabled;
        }
        let key = (workspace.clone(), id.to_owned());
        let mut state = self.lock();
        if let Some(flight) = state.flights.get(&key).map(Arc::clone) {
            // A note read mostly by joining others' reads is still in use.
            state.clock = state.clock.wrapping_add(1);
            let access = state.clock;
            if let Some(entry) = state.entries.get_mut(&key) {
                entry.access = access;
            }
            return Lookup::Wait(flight);
        }
        self.prune(&mut state);
        state.clock = state.clock.wrapping_add(1);
        let access = state.clock;
        let previous = state.entries.get_mut(&key).map(|entry| {
            entry.access = access;
            Arc::clone(&entry.snapshot)
        });
        let flight = Arc::new(Flight::default());
        state.flights.insert(key.clone(), Arc::clone(&flight));
        Lookup::Fill {
            guard: Fill {
                cache: self,
                key,
                flight,
                completed: false,
            },
            previous,
        }
    }

    /// Drops every snapshot, and detaches the reads in flight: they still
    /// answer their callers, but are not kept, and a read that starts now
    /// sends its own request instead of joining one sent before this.
    pub(super) fn invalidate(&self) {
        let mut state = self.lock();
        state.entries.clear();
        state.bytes = 0;
        state.flights.clear();
    }

    /// Records a confirmed read: detaches any read of the same note still in
    /// flight, which may have been sent before the write showed and which a
    /// read starting after this one must not join, then keeps `snapshot` if
    /// there is one. The detach holds when nothing is kept (no validator, a
    /// body over the budget), since it guards correctness, not reuse.
    pub(super) fn seed(&self, workspace: &Workspace, id: &str, snapshot: Option<Arc<Snapshot>>) {
        let key = (workspace.clone(), id.to_owned());
        let mut state = self.lock();
        state.flights.remove(&key);
        match snapshot {
            Some(snapshot) => self.store(&mut state, key, snapshot),
            None => {
                if let Some(old) = state.entries.remove(&key) {
                    state.bytes -= old.size_bytes;
                }
            }
        }
    }

    /// Drops expired entries. Linear in the entry count, which a session's
    /// working set keeps small.
    fn prune(&self, state: &mut State) {
        let mut freed = 0;
        state.entries.retain(|_, entry| {
            let keep = entry.inserted.elapsed() < self.ttl;
            if !keep {
                freed += entry.size_bytes;
            }
            keep
        });
        state.bytes -= freed;
    }

    fn store(&self, state: &mut State, key: Key, snapshot: Arc<Snapshot>) {
        if let Some(old) = state.entries.remove(&key) {
            state.bytes -= old.size_bytes;
        }
        let bytes = entry_bytes(&key, &snapshot);
        if self.disabled() || bytes > self.max_bytes || snapshot.validator.is_none() {
            return;
        }
        self.prune(state);
        while state.bytes.saturating_add(bytes) > self.max_bytes {
            let Some(key) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.access)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(old) = state.entries.remove(&key) {
                state.bytes -= old.size_bytes;
            }
        }
        state.clock = state.clock.wrapping_add(1);
        state.bytes += bytes;
        state.entries.insert(
            key,
            Entry {
                snapshot,
                size_bytes: bytes,
                inserted: tokio::time::Instant::now(),
                access: state.clock,
            },
        );
    }
}

/// The structures every entry carries whatever its contents: both `Arc`
/// headers, the snapshot, the body's `String`, and the map slot.
const FIXED_ENTRY_BYTES: usize = 2 * (2 * size_of::<usize>())
    + size_of::<Snapshot>()
    + size_of::<String>()
    + size_of::<(Key, Entry)>();

/// What one entry holds: the body, title, and validator bytes; the snapshot
/// and the body's `String`, each behind an `Arc` header; the key's strings;
/// and the map slot. The map's spare slots, which hold no bodies, are not
/// counted.
fn entry_bytes(key: &Key, snapshot: &Snapshot) -> usize {
    let team_bytes = match &key.0 {
        Workspace::Personal => 0,
        Workspace::Team { team_path } => team_path.capacity(),
    };
    snapshot
        .content
        .capacity()
        .saturating_add(snapshot.title.capacity())
        .saturating_add(
            snapshot
                .validator
                .as_ref()
                .map_or(0, |value| value.as_bytes().len()),
        )
        .saturating_add(key.1.capacity())
        .saturating_add(team_bytes)
        .saturating_add(FIXED_ENTRY_BYTES)
}

pub(super) struct Fill<'a> {
    cache: &'a NoteSnapshotCache,
    key: Key,
    flight: Arc<Flight>,
    completed: bool,
}

impl Fill<'_> {
    /// Hands `outcome` to every waiter, and keeps it unless a write detached
    /// this fill meanwhile.
    pub(super) fn complete(mut self, outcome: &Outcome) {
        let mut state = self.cache.lock();
        if state
            .flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            state.flights.remove(&self.key);
            if let Ok(snapshot) = outcome {
                self.cache
                    .store(&mut state, self.key.clone(), Arc::clone(snapshot));
            } else if let Some(old) = state.entries.remove(&self.key) {
                state.bytes -= old.size_bytes;
            }
        }
        drop(state);
        self.flight.finish(Some(outcome.clone()));
        self.completed = true;
    }
}

impl Fill<'_> {
    /// Ends the fill with nothing to keep, as when its reply was too large or
    /// had no validator: the note's older entry goes too, since this read
    /// showed it may be out of date, and waiters read again. A fill a write
    /// detached leaves the entries, and any newer fill, alone.
    pub(super) fn discard(self) {
        let mut state = self.cache.lock();
        let current = state
            .flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight));
        let old = if current {
            state.entries.remove(&self.key)
        } else {
            None
        };
        if let Some(old) = old {
            state.bytes -= old.size_bytes;
        }
        // `Drop` removes the flight and wakes the waiters.
    }
}

impl Drop for Fill<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut state = self.cache.lock();
        if state
            .flights
            .get(&self.key)
            .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight))
        {
            state.flights.remove(&self.key);
        }
        self.flight.finish(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn value(body: &str) -> Arc<Snapshot> {
        Arc::new(Snapshot::new(
            body.to_owned(),
            String::new(),
            Some(1),
            Some(HeaderValue::from_static("W/\"one\"")),
        ))
    }
    fn fill(cache: &NoteSnapshotCache, id: &str, body: &str) {
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, id) else {
            panic!("expected fill")
        };
        guard.complete(&Ok(value(body)));
    }
    fn content(cache: &NoteSnapshotCache, id: &str) -> Option<String> {
        cache
            .lock()
            .entries
            .get(&(Workspace::Personal, id.into()))
            .map(|entry| entry.snapshot.content.to_string())
    }
    #[test]
    fn byte_bound_evicts_lru_and_rejects_oversized_and_unvalidated_snapshots() {
        let size = entry_bytes(&(Workspace::Personal, "one".into()), &value("body"));
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), size * 2);
        fill(&cache, "one", "body");
        fill(&cache, "two", "body");
        let Lookup::Fill { guard, previous } = cache.begin(&Workspace::Personal, "one") else {
            panic!("fill")
        };
        assert!(previous.is_some());
        guard.complete(&Ok(value("body")));
        fill(&cache, "six", "body");
        assert_eq!(cache.lock().bytes, size * 2);
        assert_eq!(content(&cache, "two"), None);
        fill(&cache, "one", &"x".repeat(size * 3));
        assert_eq!(content(&cache, "one"), None);
        assert!(!cache.accepts(size * 3));
        let mut unvalidated = value("body");
        Arc::get_mut(&mut unvalidated).unwrap().validator = None;
        cache.seed(&Workspace::Personal, "six", Some(unvalidated));
        assert!(cache.lock().entries.is_empty());
    }
    #[test]
    fn accepts_counts_the_fixed_structures() {
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), FIXED_ENTRY_BYTES + 10);
        assert!(cache.accepts(10));
        assert!(!cache.accepts(11));
    }
    #[test]
    fn a_note_read_by_joining_stays_recent() {
        let size = entry_bytes(&(Workspace::Personal, "aaa".into()), &value("body"));
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), size * 2);
        fill(&cache, "aaa", "body");
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "aaa") else {
            panic!("fill")
        };
        fill(&cache, "bbb", "body");
        // Joining the read in flight is a use of "aaa", newer than "bbb".
        assert!(matches!(
            cache.begin(&Workspace::Personal, "aaa"),
            Lookup::Wait(_)
        ));
        fill(&cache, "ccc", "body");
        assert_eq!(content(&cache, "bbb"), None);
        assert!(content(&cache, "aaa").is_some());
        drop(guard);
    }
    #[test]
    fn disabled_cache_never_fills_or_seeds() {
        for (ttl, bytes) in [(Duration::ZERO, 1024), (Duration::from_secs(60), 0)] {
            let cache = NoteSnapshotCache::new(ttl, bytes);
            assert!(matches!(
                cache.begin(&Workspace::Personal, "id"),
                Lookup::Disabled
            ));
            assert!(!cache.accepts(1));
            cache.seed(&Workspace::Personal, "id", Some(value("body")));
            assert!(cache.lock().entries.is_empty());
        }
    }
    #[tokio::test]
    async fn a_detached_fill_answers_its_waiters_but_is_not_kept() {
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), 1024);
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        let Lookup::Wait(waiter) = cache.begin(&Workspace::Personal, "id") else {
            panic!("wait")
        };
        cache.invalidate();
        // A read starting after the write sends its own request.
        let Lookup::Fill { guard: newer, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("new fill")
        };
        newer.complete(&Ok(value("new")));
        guard.complete(&Ok(value("old")));
        let answered = waiter
            .wait()
            .await
            .expect("the waiter gets the fill's reply");
        assert_eq!(answered.unwrap().content.as_str(), "old");
        assert_eq!(content(&cache, "id").as_deref(), Some("new"));
    }
    #[test]
    fn a_seed_detaches_an_older_fill_of_the_same_note() {
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), 1024);
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        cache.seed(&Workspace::Personal, "id", Some(value("new")));
        let Lookup::Fill { previous, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("a later read must not join the older fill")
        };
        assert_eq!(previous.unwrap().content.as_str(), "new");
        guard.complete(&Ok(value("old")));
        assert_eq!(content(&cache, "id").as_deref(), Some("new"));
    }
    #[tokio::test]
    async fn a_discarded_fill_drops_the_older_entry_and_wakes_waiters() {
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), 1024);
        fill(&cache, "id", "old");
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        let Lookup::Wait(waiter) = cache.begin(&Workspace::Personal, "id") else {
            panic!("wait")
        };
        guard.discard();
        assert!(waiter.wait().await.is_none());
        assert_eq!(content(&cache, "id"), None);

        // Detached by a write, it leaves a newer fill and its entry alone.
        fill(&cache, "id", "old");
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        cache.invalidate();
        let Lookup::Fill { guard: newer, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("new fill")
        };
        guard.discard();
        assert!(matches!(
            cache.begin(&Workspace::Personal, "id"),
            Lookup::Wait(_)
        ));
        newer.complete(&Ok(value("new")));
        assert_eq!(content(&cache, "id").as_deref(), Some("new"));
    }
    #[test]
    fn a_seed_that_keeps_nothing_still_detaches() {
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), 1024);
        fill(&cache, "id", "old");
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        // A confirmed body too large to keep, or with no validator.
        cache.seed(&Workspace::Personal, "id", None);
        let Lookup::Fill { previous, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("a later read must not join the older fill")
        };
        assert!(previous.is_none(), "the older snapshot is gone too");
        drop(guard);
    }
    #[tokio::test]
    async fn cancellation_wakes_waiters_and_does_not_remove_a_newer_flight() {
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), 1024);
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        let Lookup::Wait(waiter) = cache.begin(&Workspace::Personal, "id") else {
            panic!("wait")
        };
        drop(guard);
        assert!(waiter.wait().await.is_none());
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        cache.invalidate();
        let Lookup::Fill { guard: newer, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("new fill")
        };
        drop(guard);
        assert!(matches!(
            cache.begin(&Workspace::Personal, "id"),
            Lookup::Wait(_)
        ));
        newer.complete(&Ok(value("new")));
        assert_eq!(content(&cache, "id").as_deref(), Some("new"));
    }
    #[tokio::test]
    async fn expiry_prevents_conditionals_and_workspaces_do_not_share_entries() {
        let cache = NoteSnapshotCache::new(Duration::from_millis(5), 1024);
        fill(&cache, "id", "body");
        let team = Workspace::Team {
            team_path: "team".into(),
        };
        let Lookup::Fill { guard, previous } = cache.begin(&team, "id") else {
            panic!("fill")
        };
        assert!(previous.is_none());
        drop(guard);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let Lookup::Fill { guard, previous } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        assert!(previous.is_none());
        assert_eq!(cache.lock().bytes, 0);
        drop(guard);
    }
    #[test]
    fn the_digest_is_the_body_digest() {
        assert_eq!(value("body").digest(), crate::hash::body_digest("body"));
    }
}
