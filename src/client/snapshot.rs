//! Conditional body snapshots. A cached value is never served without a GET.
use super::HackmdError;
use crate::models::Workspace;
use reqwest::header::HeaderValue;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

type Key = (Workspace, String);
type Outcome = Result<Arc<Snapshot>, HackmdError>;

// No private note text appears in Debug output.
pub(crate) struct Snapshot {
    pub(crate) content: Arc<String>,
    pub(crate) digest: [u8; 32],
    pub(crate) title: String,
    pub(crate) last_changed_at: Option<i64>,
    pub(super) validator: Option<HeaderValue>,
}

impl Snapshot {
    fn retained_bytes(&self) -> usize {
        self.content
            .capacity()
            .saturating_add(self.title.capacity())
            .saturating_add(
                self.validator
                    .as_ref()
                    .map_or(0, |value| value.as_bytes().len()),
            )
            .saturating_add(std::mem::size_of::<Self>())
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
    generation: u64,
    access: u64,
}

#[derive(Debug, Default)]
struct State {
    generation: u64,
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

    pub(super) fn begin(&self, workspace: &Workspace, id: &str) -> Lookup<'_> {
        if self.max_bytes == 0 || self.ttl.is_zero() {
            return Lookup::Disabled;
        }
        let key = (workspace.clone(), id.to_owned());
        let mut state = self.lock();
        if let Some(flight) = state.flights.get(&key) {
            return Lookup::Wait(Arc::clone(flight));
        }
        let generation = state.generation;
        state.clock = state.clock.wrapping_add(1);
        let access = state.clock;
        state
            .entries
            .retain(|_, entry| entry.inserted.elapsed() < self.ttl);
        state.bytes = state.entries.values().map(|entry| entry.size_bytes).sum();
        let previous = state
            .entries
            .get_mut(&key)
            .filter(|entry| entry.generation == generation)
            .map(|entry| {
                entry.access = access;
                Arc::clone(&entry.snapshot)
            });
        let flight = Arc::new(Flight::default());
        state.flights.insert(key.clone(), Arc::clone(&flight));
        Lookup::Fill {
            guard: Fill {
                cache: self,
                key,
                generation,
                flight,
                completed: false,
            },
            previous,
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Also fences fills that began before a write, including cancelled writes.
    pub(super) fn invalidate(&self) {
        let mut state = self.lock();
        state.generation = state.generation.wrapping_add(1);
        state.entries.clear();
        state.bytes = 0;
        for (_, flight) in state.flights.drain() {
            flight.finish(None);
        }
    }

    pub(super) fn seed(
        &self,
        workspace: &Workspace,
        id: &str,
        generation: u64,
        snapshot: Arc<Snapshot>,
    ) {
        let mut state = self.lock();
        if state.generation == generation {
            let key = (workspace.clone(), id.to_owned());
            if let Some(flight) = state.flights.remove(&key) {
                flight.finish(None);
            }
            self.store(&mut state, key, snapshot);
        }
    }

    fn store(&self, state: &mut State, key: Key, snapshot: Arc<Snapshot>) {
        if let Some(old) = state.entries.remove(&key) {
            state.bytes -= old.size_bytes;
        }
        let bytes = entry_bytes(&key, &snapshot);
        // Charge metadata too, bounding empty notes and unusually long titles.
        if self.max_bytes == 0
            || self.ttl.is_zero()
            || bytes > self.max_bytes
            || snapshot.validator.is_none()
        {
            return;
        }
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
                generation: state.generation,
                access: state.clock,
            },
        );
    }
}

fn entry_bytes(key: &Key, snapshot: &Snapshot) -> usize {
    let team_bytes = match &key.0 {
        Workspace::Personal => 0,
        Workspace::Team { team_path } => team_path.capacity(),
    };
    snapshot
        .retained_bytes()
        .saturating_add(key.1.capacity())
        .saturating_add(team_bytes)
        .saturating_add(std::mem::size_of::<Key>())
        .saturating_add(std::mem::size_of::<Entry>())
}

pub(super) struct Fill<'a> {
    cache: &'a NoteSnapshotCache,
    key: Key,
    generation: u64,
    flight: Arc<Flight>,
    completed: bool,
}

impl Fill<'_> {
    pub(super) fn complete(mut self, outcome: &Outcome) -> bool {
        let mut state = self.cache.lock();
        let current = state.generation == self.generation
            && state
                .flights
                .get(&self.key)
                .is_some_and(|flight| Arc::ptr_eq(flight, &self.flight));
        if current {
            state.flights.remove(&self.key);
            if let Ok(snapshot) = outcome {
                self.cache
                    .store(&mut state, self.key.clone(), Arc::clone(snapshot));
            } else if let Some(old) = state.entries.remove(&self.key) {
                state.bytes -= old.size_bytes;
            }
        }
        self.flight.finish(current.then(|| outcome.clone()));
        self.completed = true;
        current
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
        Arc::new(Snapshot {
            content: Arc::new(body.to_owned()),
            digest: crate::hash::body_digest(body),
            title: String::new(),
            last_changed_at: Some(1),
            validator: Some(HeaderValue::from_static("W/\"one\"")),
        })
    }
    fn fill(cache: &NoteSnapshotCache, id: &str, body: &str) {
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, id) else {
            panic!("expected fill")
        };
        assert!(guard.complete(&Ok(value(body))));
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
        assert!(guard.complete(&Ok(value("body"))));
        fill(&cache, "six", "body");
        let state = cache.lock();
        assert_eq!(state.bytes, size * 2);
        assert!(
            !state
                .entries
                .contains_key(&(Workspace::Personal, "two".into()))
        );
        drop(state);
        fill(&cache, "one", &"x".repeat(size * 3));
        assert!(
            !cache
                .lock()
                .entries
                .contains_key(&(Workspace::Personal, "one".into()))
        );
        let mut unvalidated = value("body");
        Arc::get_mut(&mut unvalidated).unwrap().validator = None;
        cache.seed(&Workspace::Personal, "six", cache.generation(), unvalidated);
        assert!(cache.lock().entries.is_empty());
    }
    #[test]
    fn disabled_cache_never_fills_or_seeds() {
        for (ttl, bytes) in [(Duration::ZERO, 1024), (Duration::from_secs(60), 0)] {
            let cache = NoteSnapshotCache::new(ttl, bytes);
            assert!(matches!(
                cache.begin(&Workspace::Personal, "id"),
                Lookup::Disabled
            ));
            cache.seed(
                &Workspace::Personal,
                "id",
                cache.generation(),
                value("body"),
            );
            assert!(cache.lock().entries.is_empty());
        }
    }
    #[tokio::test]
    async fn invalidation_fences_fill_seed_and_waiters() {
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), 1024);
        let generation = cache.generation();
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        let Lookup::Wait(waiter) = cache.begin(&Workspace::Personal, "id") else {
            panic!("wait")
        };
        cache.invalidate();
        assert!(waiter.wait().await.is_none());
        let Lookup::Fill { guard: newer, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("new fill")
        };
        assert!(newer.complete(&Ok(value("new"))));
        assert!(!guard.complete(&Ok(value("old"))));
        cache.seed(&Workspace::Personal, "id", generation, value("old"));
        assert_eq!(
            &*cache.lock().entries[&(Workspace::Personal, "id".into())]
                .snapshot
                .content,
            "new"
        );
    }
    #[tokio::test]
    async fn confirmed_seed_fences_an_older_fill_for_the_same_note() {
        let cache = NoteSnapshotCache::new(Duration::from_secs(60), 1024);
        let Lookup::Fill { guard, .. } = cache.begin(&Workspace::Personal, "id") else {
            panic!("fill")
        };
        let Lookup::Wait(waiter) = cache.begin(&Workspace::Personal, "id") else {
            panic!("wait")
        };
        cache.seed(
            &Workspace::Personal,
            "id",
            cache.generation(),
            value("confirmed"),
        );
        assert!(waiter.wait().await.is_none());
        assert!(!guard.complete(&Ok(value("old"))));
        assert_eq!(
            &*cache.lock().entries[&(Workspace::Personal, "id".into())]
                .snapshot
                .content,
            "confirmed"
        );
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
        assert!(newer.complete(&Ok(value("new"))));
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
}
