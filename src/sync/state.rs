use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use sha2::{Digest, Sha256};

use crate::{
    hash::{body_digest, body_hash, body_hash_from_digest, push_hex},
    models::Workspace,
};

/// Persistent metadata for one locally tracked note.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "StoredState", into = "StoredState")]
pub(crate) struct TrackedNoteState {
    pub(crate) internal_id: String,
    pub(crate) workspace: Workspace,
    /// The tracked file, canonicalized: what lookups match and what results
    /// report.
    pub(crate) local_path: PathBuf,
    pub(crate) baseline_body_hash: String,
    pub(crate) last_observed_remote_timestamp: String,
    /// Hash of the `*.remote.md` a conflicted push last wrote. A later
    /// conflict replaces that file only while it still has exactly this
    /// content, so a file the user wrote or edited is never overwritten.
    pub(crate) remote_snapshot_hash: Option<String>,
}

/// A sidecar as written. The path appears twice, once inside
/// `local_file_identity`, because older builds look files up there; in
/// memory it is one field, so the two can never be read apart. A sidecar
/// whose copies disagree was edited by hand or torn, and is refused as
/// corrupt rather than trusted by either copy.
#[derive(Deserialize, Serialize)]
struct StoredState {
    internal_id: String,
    #[serde(with = "crate::models::tagged")]
    workspace: Workspace,
    local_path: PathBuf,
    baseline_body_hash: String,
    last_observed_remote_timestamp: String,
    local_file_identity: LocalFileIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote_snapshot_hash: Option<String>,
}

impl TryFrom<StoredState> for TrackedNoteState {
    type Error = &'static str;

    fn try_from(stored: StoredState) -> Result<Self, Self::Error> {
        if stored.local_file_identity.canonical_path != stored.local_path {
            return Err("local_path and local_file_identity name different files");
        }
        Ok(Self {
            internal_id: stored.internal_id,
            workspace: stored.workspace,
            local_path: stored.local_path,
            baseline_body_hash: stored.baseline_body_hash,
            last_observed_remote_timestamp: stored.last_observed_remote_timestamp,
            remote_snapshot_hash: stored.remote_snapshot_hash,
        })
    }
}

impl From<TrackedNoteState> for StoredState {
    fn from(state: TrackedNoteState) -> Self {
        Self {
            local_file_identity: LocalFileIdentity {
                canonical_path: state.local_path.clone(),
            },
            internal_id: state.internal_id,
            workspace: state.workspace,
            local_path: state.local_path,
            baseline_body_hash: state.baseline_body_hash,
            last_observed_remote_timestamp: state.last_observed_remote_timestamp,
            remote_snapshot_hash: state.remote_snapshot_hash,
        }
    }
}

impl TrackedNoteState {
    /// Records the note as tracked at the moment `baseline_body` is what both
    /// `local_path` and `HackMD` hold. Every sync tool goes through here so the
    /// stored hash and file identity can never disagree with the baseline.
    /// The record keeps `local_path` canonicalized, however the caller spelled
    /// it. Hashes the body and touches the filesystem, so async callers run it
    /// through `local::offload`.
    pub(crate) fn capture(
        internal_id: String,
        workspace: Workspace,
        local_path: impl AsRef<Path>,
        baseline_body: &str,
        remote_timestamp: Option<i64>,
    ) -> Result<Self, StateError> {
        Ok(Self {
            local_path: fs::canonicalize(local_path.as_ref())?,
            internal_id,
            workspace,
            baseline_body_hash: body_hash(baseline_body),
            last_observed_remote_timestamp: timestamp_text(remote_timestamp),
            remote_snapshot_hash: None,
        })
    }

    /// Re-captures this note after a successful push: same note and file, new
    /// baseline body.
    pub(crate) fn advance(
        self,
        baseline_body: &str,
        remote_timestamp: Option<i64>,
    ) -> Result<Self, StateError> {
        Self::capture(
            self.internal_id,
            self.workspace,
            &self.local_path,
            baseline_body,
            remote_timestamp,
        )
    }
}

/// Renders a `HackMD` millisecond timestamp for the sidecar and the sync tools,
/// which report it as text because the API omits it on some responses.
pub(crate) fn timestamp_text(value: Option<i64>) -> String {
    value.map_or_else(String::new, |timestamp| timestamp.to_string())
}

/// The sidecar filename stem for a note, and the body of its by-path pointer.
fn state_key(workspace: &Workspace, internal_id: &str) -> String {
    key_with(workspace, internal_id, false)
}

/// A key under either encoding: `legacy` keeps `-` bare, as older builds did.
/// That encoding is only computed to find their records, and a record found
/// that way is checked against its own identity, since two notes can share
/// a legacy key.
fn key_with(workspace: &Workspace, internal_id: &str, legacy: bool) -> String {
    let encode = |value| encode_component(value, legacy);
    match workspace {
        Workspace::Personal => format!("personal--{}", encode(internal_id)),
        Workspace::Team { team_path } => {
            format!("team--{}--{}", encode(team_path), encode(internal_id))
        }
    }
}

/// Where an older build looks for the tracked file's path in a sidecar.
#[derive(Deserialize, Serialize)]
///
/// Sidecars written by older builds also carry `device_id` and `file_id`.
/// Nothing ever read them, and every atomic rename changes the inode anyway,
/// so they are no longer written; loading simply ignores them.
struct LocalFileIdentity {
    canonical_path: PathBuf,
}

/// Local state layout. Constructing it never touches the filesystem.
#[derive(Debug)]
pub(crate) struct StateStore {
    root: PathBuf,
}

#[derive(Debug)]
pub(crate) struct LoadedTrackedState {
    pub(crate) state: TrackedNoteState,
    pub(crate) baseline_body: String,
    pub(crate) baseline_digest: [u8; 32],
    pub(crate) baseline_path: PathBuf,
}

impl StateStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Runs `work` off the async runtime once the store is known to be
    /// trustworthy. Every public method goes through here, so none can read
    /// a record someone else planted.
    fn guarded<T>(&self, work: impl FnOnce() -> Result<T, StateError>) -> Result<T, StateError> {
        crate::local::offload(|| {
            self.check_trusted()?;
            work()
        })
    }

    /// Refuses an untrustworthy store before a caller touches anything else.
    pub(crate) fn trusted(&self) -> Result<(), StateError> {
        self.guarded(|| Ok(()))
    }

    /// Refuses a state directory another account could have written into:
    /// a sidecar planted there maps a local file to a note of the planter's
    /// choosing, and the next push sends that file there. A directory not
    /// yet created is fine, since this server creates it owner-only.
    fn check_trusted(&self) -> Result<(), StateError> {
        match untrusted_reason(&self.root) {
            Some(reason) => Err(StateError::UntrustedStateDir {
                path: self.root.clone(),
                reason,
            }),
            None => Ok(()),
        }
    }

    pub(crate) fn probe_writable(&self) -> Result<(), StateError> {
        self.check_trusted()?;
        create_private_dir_all(&self.root)?;
        let temporary = NamedTempFile::new_in(&self.root)?;
        set_private_permissions(temporary.as_file())?;
        temporary.as_file().sync_all()?;
        Ok(())
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    #[cfg(test)]
    fn paths_for(&self, workspace: &Workspace, internal_id: &str) -> StatePaths {
        self.paths_for_key(&state_key(workspace, internal_id))
    }

    fn paths_for_key(&self, key: &str) -> StatePaths {
        let tracked = self.root.join("tracked");
        StatePaths {
            sidecar: tracked.join(format!("{key}.json")),
            baseline: tracked.join(format!("{key}.baseline.md")),
        }
    }

    /// Where the pointer from a local file back to its sidecar lives. The name
    /// is a hash so any path, however long or oddly encoded, maps to one file.
    fn index_path(&self, canonical: &Path) -> PathBuf {
        let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
        let mut name = String::with_capacity(64);
        push_hex(&mut name, &digest);
        self.root.join("by-path").join(name)
    }

    /// Persists sync state atomically per file. Only pull/push handlers should
    /// call this operation; server startup constructs the store without writes.
    ///
    /// Baseline and sidecar are two renames, not one transaction. A crash
    /// between them leaves a pair whose hashes disagree, which
    /// `load_for_local_path` refuses on the next call. The note is re-pulled
    /// rather than synced against a baseline nobody can vouch for.
    pub(crate) fn persist_from_sync(
        &self,
        state: &TrackedNoteState,
        baseline_body: &str,
    ) -> Result<(), StateError> {
        self.guarded(|| {
            let key = state_key(&state.workspace, &state.internal_id);
            let paths = self.paths_for_key(&key);
            let canonical = &state.local_path;
            let is_this_note = |record: &StoredRecord| {
                record.state.workspace == state.workspace
                    && record.state.internal_id == state.internal_id
            };

            // One pass over the store finds this note's record, under its
            // current key or one an older build wrote, and every other note
            // still naming this file. A sidecar that does not parse is skipped
            // by the scan and written over below: that is the repair a re-pull
            // exists for.
            let records = self.scan()?;
            let existing = records
                .iter()
                .filter(|record| is_this_note(record))
                .max_by_key(|record| record.key == key);
            let mut state = state.clone();
            if let Some(existing) = existing {
                if existing.state.local_path == *canonical {
                    // Same note, same file: the conflict snapshot beside it is
                    // still the one this tool wrote, whoever captured this
                    // state.
                    if state.remote_snapshot_hash.is_none() {
                        state
                            .remote_snapshot_hash
                            .clone_from(&existing.state.remote_snapshot_hash);
                    }
                } else {
                    self.remove_hint_if_matches(&existing.state.local_path, &existing.key)?;
                }
            }

            // One file, one record. Any other note still naming this file was
            // displaced by the write that brought this body here; left behind,
            // it could win a later lookup and send this body to that note.
            // Removed first, so a crash below leaves the file untracked rather
            // than tracked by the wrong note.
            for other in &records {
                if other.state.local_path == *canonical && !is_this_note(other) {
                    remove_record(&other.paths)?;
                }
            }

            write_private_atomic(&paths.baseline, baseline_body.as_bytes())?;
            write_private_atomic(&paths.sidecar, &serde_json::to_vec_pretty(&state)?)?;

            // This note's records under any other key, from an older build, are
            // superseded now. All of them, not just the one read above: a move
            // interrupted before this point leaves both keys behind, and a
            // leftover would make every later scan of this file ambiguous.
            for stale in records
                .iter()
                .filter(|record| is_this_note(record) && record.key != key)
            {
                remove_record(&stale.paths)?;
            }

            // A hint, not a source of truth: the loader verifies what it finds
            // and falls back to a scan, so a stale or missing pointer costs
            // speed and never correctness. That is also why it is written
            // without the fsync the sidecar pair gets.
            write_private_hint(&self.index_path(canonical), key.as_bytes())?;
            Ok(())
        })
    }

    /// Rewrites only the sidecar of a record that is already persisted, for a
    /// field outside the baseline pair such as the snapshot hash. The baseline
    /// and its hash are untouched, so the pair stays consistent, and a body of
    /// up to 50 MiB is not rewritten to change one field.
    pub(crate) fn update_sidecar(&self, state: &TrackedNoteState) -> Result<(), StateError> {
        self.guarded(|| {
            let paths = self
                .resolve(&state.workspace, &state.internal_id)?
                .ok_or(StateError::NotTracked)?
                .paths;
            write_private_atomic(&paths.sidecar, &serde_json::to_vec_pretty(state)?)?;
            Ok(())
        })
    }

    /// The stored record for one note, under the current key or, failing
    /// that, the key an older build wrote. A record whose contents name a
    /// different note is refused: a legacy key can be shared by two notes.
    fn resolve(
        &self,
        workspace: &Workspace,
        internal_id: &str,
    ) -> Result<Option<StoredRecord>, StateError> {
        let current = state_key(workspace, internal_id);
        let legacy = key_with(workspace, internal_id, true);
        // Without a `-` in either component the two keys are the same file.
        let legacy = (legacy != current).then_some(legacy);
        for (key, is_current) in
            std::iter::once((current, true)).chain(legacy.map(|key| (key, false)))
        {
            let paths = self.paths_for_key(&key);
            let Some(sidecar) = read_if_present(&paths.sidecar)? else {
                continue;
            };
            let state: TrackedNoteState = serde_json::from_slice(&sidecar)?;
            if state.workspace != *workspace || state.internal_id != internal_id {
                // Another note under a legacy key is not this note's record;
                // under the current key it is corruption.
                if is_current {
                    return Err(StateError::StateIdentityMismatch);
                }
                continue;
            }
            return Ok(Some(StoredRecord { key, paths, state }));
        }
        Ok(None)
    }

    /// The record naming `local_path`, if any, without reading or verifying
    /// its baseline: enough to compare a file against the recorded hash, not
    /// to sync from.
    pub(crate) fn record_for(
        &self,
        local_path: &Path,
    ) -> Result<Option<TrackedNoteState>, StateError> {
        self.guarded(|| {
            let canonical = match fs::canonicalize(local_path) {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(StateError::Io(error)),
            };
            Ok(self.find(&canonical)?.map(|record| record.state))
        })
    }

    /// Reads every tracked sidecar without touching the working Markdown or
    /// baseline files. Callers use this for state discovery, not sync safety;
    /// an individual sync still verifies its baseline before comparison.
    pub(crate) fn list_tracked(&self) -> Result<Vec<TrackedNoteState>, StateError> {
        self.guarded(|| {
            Ok(self
                .scan()?
                .into_iter()
                .map(|record| record.state)
                .collect())
        })
    }

    /// Every readable sidecar, with the paths it was actually found at rather
    /// than ones re-derived from its contents, so records under keys written
    /// by older builds stay reachable.
    fn scan(&self) -> Result<Vec<StoredRecord>, StateError> {
        let tracked = self.root.join("tracked");
        let entries = match fs::read_dir(tracked) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(StateError::Io(error)),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry?;

            // This server writes only regular files here; anything else, a FIFO
            // above all, would only hang the read below.
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            let Some(key) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            // A sidecar removed since the listing is simply gone.
            let Some(bytes) = read_if_present(&path)? else {
                continue;
            };
            match serde_json::from_slice(&bytes) {
                Ok(state) => records.push(StoredRecord {
                    key: key.to_owned(),
                    paths: self.paths_for_key(key),
                    state,
                }),
                Err(_) => {
                    tracing::warn!(path = %path.display(), "skipping malformed tracked sidecar");
                }
            }
        }
        Ok(records)
    }

    /// Stops tracking exactly one workspace/note pair. The working Markdown
    /// path in the sidecar is used only to locate its rebuildable index hint;
    /// the working file itself is never opened, changed, or removed.
    pub(crate) fn untrack(
        &self,
        workspace: &Workspace,
        internal_id: &str,
    ) -> Result<TrackedNoteState, StateError> {
        self.guarded(|| {
            let record = self
                .resolve(workspace, internal_id)?
                .ok_or(StateError::NotTracked)?;
            self.remove_hint_if_matches(&record.state.local_path, &record.key)?;
            remove_record(&record.paths)?;
            Ok(record.state)
        })
    }

    fn remove_hint_if_matches(
        &self,
        canonical: &Path,
        expected_key: &str,
    ) -> Result<(), StateError> {
        let hint = self.index_path(canonical);
        match fs::read_to_string(&hint) {
            Ok(key) if key.trim() == expected_key => remove_if_present(&hint),
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StateError::Io(error)),
        }
    }

    /// Follows the by-path pointer, if one is there and still describes this
    /// file. `None` means the caller should scan, which also covers state
    /// written before the index existed.
    fn find_via_index(&self, canonical: &Path) -> Result<Option<StoredRecord>, StateError> {
        let Ok(key) = fs::read_to_string(self.index_path(canonical)) else {
            return Ok(None);
        };
        let key = key.trim();
        if !valid_state_key(key) {
            return Ok(None);
        }
        let paths = self.paths_for_key(key);
        let corrupt = || StateError::CorruptTrackedState {
            sidecar_path: paths.sidecar.clone(),
            local_path: canonical.to_path_buf(),
        };
        let Some(sidecar) = read_if_present(&paths.sidecar).map_err(|_| corrupt())? else {
            return Ok(None);
        };
        let state: TrackedNoteState = serde_json::from_slice(&sidecar).map_err(|_| corrupt())?;
        if state.local_path != canonical {
            return Ok(None);
        }
        Ok(Some(StoredRecord {
            key: key.to_owned(),
            paths,
            state,
        }))
    }

    /// Scans every sidecar for the one recording `canonical`, for state the
    /// index does not point at. Two records naming one file cannot be told
    /// apart safely, so that is refused rather than resolved by scan order.
    fn find_by_scan(&self, canonical: &Path) -> Result<Option<StoredRecord>, StateError> {
        let mut matches = self
            .scan()?
            .into_iter()
            .filter(|record| record.state.local_path == canonical);
        let Some(found) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err(StateError::AmbiguousTrackedState {
                local_path: canonical.to_path_buf(),
            });
        }
        Ok(Some(found))
    }

    /// The record whose file is `canonical`: the by-path pointer answers in
    /// one read, and failing that the sidecars are scanned. Neither derives a
    /// filename from the path, so a note stays tracked when the caller reaches
    /// it through a symlink or a differently spelled path. A pointer at a
    /// sidecar that cannot be read is reported only if the scan finds nothing.
    fn find(&self, canonical: &Path) -> Result<Option<StoredRecord>, StateError> {
        let indexed_error = match self.find_via_index(canonical) {
            Ok(Some(found)) => return Ok(Some(found)),
            Ok(None) => None,
            Err(error) => Some(error),
        };
        match self.find_by_scan(canonical)? {
            Some(found) => Ok(Some(found)),
            None => indexed_error.map_or(Ok(None), Err),
        }
    }

    /// Finds the tracked note whose recorded file is `local_path`, with its
    /// baseline read and verified against the recorded hash.
    pub(crate) fn load_for_local_path(
        &self,
        local_path: &Path,
    ) -> Result<LoadedTrackedState, StateError> {
        self.guarded(|| {
            let canonical = fs::canonicalize(local_path)?;
            let record = self.find(&canonical)?.ok_or(StateError::NotTracked)?;
            Self::load_verified(record.state, record.paths)
        })
    }

    fn load_verified(
        state: TrackedNoteState,
        paths: StatePaths,
    ) -> Result<LoadedTrackedState, StateError> {
        let baseline_body = fs::read_to_string(&paths.baseline).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                StateError::MissingBaseline {
                    baseline_path: paths.baseline.clone(),
                    note_id: state.internal_id.clone(),
                    workspace: state.workspace.clone(),
                    local_path: state.local_path.clone(),
                }
            } else {
                StateError::Io(error)
            }
        })?;

        // A torn or hand-edited pair would silently corrupt every later
        // three-way comparison, so refuse the state instead of guessing.
        let baseline_digest = body_digest(&baseline_body);
        if body_hash_from_digest(&baseline_digest) != state.baseline_body_hash {
            return Err(StateError::BaselineMismatch {
                baseline_path: paths.baseline,
                note_id: state.internal_id,
                workspace: state.workspace,
                local_path: state.local_path,
            });
        }
        Ok(LoadedTrackedState {
            state,
            baseline_body,
            baseline_digest,
            baseline_path: paths.baseline,
        })
    }
}

struct StatePaths {
    sidecar: PathBuf,
    baseline: PathBuf,
}

/// A sidecar as found on disk: its key, where it and its baseline live, and
/// what it records.
struct StoredRecord {
    key: String,
    paths: StatePaths,
    state: TrackedNoteState,
}

/// Escapes everything but `[A-Za-z0-9_.]`. `-` is escaped too, so the `--`
/// separating a key's components can never also come from inside one: team
/// `a--b` with note `c` and team `a` with note `b--c` get different keys.
/// `keep_hyphen` reproduces the older encoding that left `-` bare.
fn encode_component(value: &str, keep_hyphen: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'_' | b'.')
            || (keep_hyphen && byte == b'-')
        {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

fn valid_state_key(key: &str) -> bool {
    if !(key.starts_with("personal--") || key.starts_with("team--")) {
        return false;
    }
    let bytes = key.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            index += 1;
        } else {
            return false;
        }
    }
    true
}

fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<(), StateError> {
    let parent = path.parent().ok_or(StateError::InvalidStatePath)?;
    create_private_dir_all(parent)?;
    crate::local::replace_atomic(path, contents, true, set_private_permissions)?;
    Ok(())
}

/// Writes a file the server can rebuild, so it skips the durability the sidecar
/// pair needs: losing it after a crash costs one directory scan.
fn write_private_hint(path: &Path, contents: &[u8]) -> Result<(), StateError> {
    let parent = path.parent().ok_or(StateError::InvalidStatePath)?;
    create_private_dir_all(parent)?;
    let file = fs::File::create(path)?;
    set_private_permissions(&file)?;
    (&file).write_all(contents)?;
    Ok(())
}

/// Creates the state tree owner-only. The sidecar files are already 0600, but
/// their names spell out note IDs and team paths, so the directory listing is
/// worth hiding from other local accounts too.
fn create_private_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

#[cfg(unix)]
fn set_private_permissions(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum StateError {
    #[error("local Markdown file is not tracked; pull it before sync operations")]
    NotTracked,
    #[error("local sync state path has no parent directory; check HACKMD_MCP_STATE_DIR")]
    InvalidStatePath,
    #[error("local sync state I/O failed: {0}; check that HACKMD_MCP_STATE_DIR is writable")]
    Io(#[from] io::Error),
    #[error("local sync state could not be encoded: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(
        "HACKMD_MCP_STATE_DIR {} {reason}; point it at a directory only this account can write",
        path.display()
    )]
    UntrustedStateDir { path: PathBuf, reason: &'static str },
    #[error(
        "tracked sidecar {} for {} is corrupt; re-pull the note or untrack this record",
        sidecar_path.display(),
        local_path.display()
    )]
    CorruptTrackedState {
        sidecar_path: PathBuf,
        local_path: PathBuf,
    },
    #[error(
        "tracked sidecar identity does not match its filename; untrack this record and re-pull the note"
    )]
    StateIdentityMismatch,
    #[error(
        "more than one tracked note records {}; untrack the stale ones with hackmd_untrack_note, then re-pull",
        local_path.display()
    )]
    AmbiguousTrackedState { local_path: PathBuf },
    #[error(
        "tracked baseline {} is missing; re-pull note {note_id} from {workspace} to {} before syncing",
        baseline_path.display(),
        local_path.display()
    )]
    MissingBaseline {
        baseline_path: PathBuf,
        note_id: String,
        workspace: Workspace,
        local_path: PathBuf,
    },
    #[error(
        "tracked baseline {} does not match its recorded hash; re-pull note {note_id} from {workspace} to {} before syncing",
        baseline_path.display(),
        local_path.display()
    )]
    BaselineMismatch {
        baseline_path: PathBuf,
        note_id: String,
        workspace: Workspace,
        local_path: PathBuf,
    },
}

impl crate::reply::ToolError for StateError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::NotTracked => ErrorKind::NotTracked,
            Self::InvalidStatePath | Self::Io(_) | Self::Serialize(_) => ErrorKind::LocalIo,
            Self::UntrustedStateDir { .. } => ErrorKind::LocalAccess,
            Self::CorruptTrackedState { .. }
            | Self::StateIdentityMismatch
            | Self::AmbiguousTrackedState { .. }
            | Self::MissingBaseline { .. }
            | Self::BaselineMismatch { .. } => ErrorKind::SyncState,
        }
    }
}

/// Removes a record's pair, baseline first: a sidecar left without its
/// baseline is refused on load, while the reverse would be a baseline
/// nothing points at.
fn remove_record(paths: &StatePaths) -> Result<(), StateError> {
    remove_if_present(&paths.baseline)?;
    remove_if_present(&paths.sidecar)
}

#[cfg(unix)]
fn untrusted_reason(root: &Path) -> Option<&'static str> {
    use std::os::unix::fs::MetadataExt;
    use std::sync::OnceLock;

    // The account this server runs as, read off a file it just created: asking
    // the OS directly takes `unsafe`, which this crate forbids. Only an answer
    // is cached; a failed probe is tried again next time.
    static OWN_UID: OnceLock<u32> = OnceLock::new();

    // A missing directory is fine: this server creates it owner-only. Any other
    // failure to look is a failure to vouch for it.
    let metadata = match fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => return Some("could not be checked: its metadata could not be read"),
    };

    // The probe goes to the system temporary directory, never into the
    // directory under judgment, and a failed probe fails closed.
    let own = if let Some(uid) = OWN_UID.get() {
        *uid
    } else {
        let Ok(uid) = tempfile::tempfile()
            .and_then(|file| file.metadata())
            .map(|metadata| metadata.uid())
        else {
            return Some("could not be checked: no temporary file could be created");
        };
        *OWN_UID.get_or_init(|| uid)
    };

    // The root and the directories records live in are held to one rule: owned
    // by this account, and not writable by its group or anyone else, since a
    // group member could plant records as readily as a stranger. The record
    // directories must also be real directories, since a symlink could lead
    // anywhere.
    let mut directories = vec![metadata];
    for name in ["tracked", "by-path"] {
        match fs::symlink_metadata(root.join(name)) {
            Ok(inner) if inner.is_dir() => directories.push(inner),
            Ok(_) => return Some("holds a record directory that is not a plain directory"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Some("could not be checked: a record directory could not be read"),
        }
    }
    for directory in &directories {
        if directory.uid() != own {
            return Some("is owned by another account, or holds a directory that is");
        }
        if directory.mode() & 0o022 != 0 {
            return Some(
                "is writable by other accounts, or holds a directory that is; chmod -R go-w it",
            );
        }
    }
    None
}

#[cfg(not(unix))]
fn untrusted_reason(_root: &Path) -> Option<&'static str> {
    None
}

/// A file's bytes, or `None` if it is not there.
fn read_if_present(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn remove_if_present(path: &Path) -> Result<(), StateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::{StateError, StateStore, TrackedNoteState};
    use crate::models::Workspace;

    fn fixture_state(workspace: Workspace) -> TrackedNoteState {
        TrackedNoteState {
            internal_id: "note/id".to_owned(),
            workspace,
            local_path: PathBuf::from("/tmp/note.md"),
            baseline_body_hash: "sha256:fixture".to_owned(),
            last_observed_remote_timestamp: "2026-08-29T00:00:00Z".to_owned(),
            remote_snapshot_hash: None,
        }
    }

    #[test]
    fn sidecars_from_older_builds_still_load() {
        let legacy = serde_json::json!({
            "internal_id": "note/id",
            "workspace": {"kind": "personal"},
            "local_path": "/tmp/note.md",
            "baseline_body_hash": "sha256:fixture",
            "last_observed_remote_timestamp": "2026-08-29T00:00:00Z",
            "local_file_identity": {
                "canonical_path": "/tmp/note.md",
                "device_id": 1,
                "file_id": 2
            }
        });
        assert_eq!(
            serde_json::from_value::<TrackedNoteState>(legacy)
                .expect("legacy sidecar should deserialize"),
            fixture_state(Workspace::Personal)
        );
    }

    /// The path is written twice so older builds still find it; a sidecar
    /// whose copies disagree cannot say which file it tracks.
    #[test]
    fn the_path_is_written_twice_and_must_agree() {
        let written = serde_json::to_value(fixture_state(Workspace::Personal))
            .expect("state should serialize");
        assert_eq!(written["local_path"], "/tmp/note.md");
        assert_eq!(
            written["local_file_identity"]["canonical_path"],
            "/tmp/note.md"
        );

        let mut torn = written;
        torn["local_file_identity"]["canonical_path"] = "/tmp/other.md".into();
        let parsed = serde_json::from_value::<TrackedNoteState>(torn);
        assert!(parsed.is_err(), "{parsed:?}");
    }

    #[test]
    fn a_fresh_capture_of_the_same_file_keeps_the_snapshot_hash() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let capture = || {
            TrackedNoteState::capture(
                "note/id".to_owned(),
                Workspace::Personal,
                local_path.clone(),
                "baseline",
                Some(1),
            )
            .expect("fixture state should capture")
        };
        store
            .persist_from_sync(&capture(), "baseline")
            .expect("state should persist");
        let mut with_snapshot = capture();
        with_snapshot.remote_snapshot_hash = Some("sha256:snapshot".to_owned());
        store
            .update_sidecar(&with_snapshot)
            .expect("sidecar should update");

        // A re-pull or an advance captures afresh, without the hash.
        store
            .persist_from_sync(&capture(), "baseline")
            .expect("state should persist again");
        assert_eq!(
            store
                .load_for_local_path(&local_path)
                .expect("state should load")
                .state
                .remote_snapshot_hash
                .as_deref(),
            Some("sha256:snapshot")
        );
    }

    #[test]
    fn lookup_uses_the_index_and_survives_losing_it() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        store
            .persist_from_sync(
                &TrackedNoteState::capture(
                    "note/id".to_owned(),
                    Workspace::Personal,
                    local_path.clone(),
                    "baseline",
                    Some(1),
                )
                .expect("fixture state should capture"),
                "baseline",
            )
            .expect("state should persist");

        let loaded = store
            .load_for_local_path(&local_path)
            .expect("indexed lookup should find the note");
        assert_eq!(loaded.state.internal_id, "note/id");

        // Delete the pointer: the scan still finds it, which is what keeps
        // state written by older builds loadable.
        fs::remove_dir_all(directory.path().join("state/by-path"))
            .expect("index should be removable");
        let loaded = store
            .load_for_local_path(&local_path)
            .expect("scan should still find the note");
        assert_eq!(loaded.state.internal_id, "note/id");

        // A pointer aimed at a note that describes some other file is ignored.
        let other = directory.path().join("other.md");
        fs::write(&other, "baseline").expect("other fixture should write");
        assert!(matches!(
            store.load_for_local_path(&other),
            Err(StateError::NotTracked)
        ));
    }

    #[test]
    fn missing_baseline_has_a_recovery_error() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let state = TrackedNoteState::capture(
            "note-id".to_owned(),
            Workspace::Personal,
            local_path.clone(),
            "baseline",
            Some(1),
        )
        .expect("fixture state should capture");
        store
            .persist_from_sync(&state, "baseline")
            .expect("state should persist");
        fs::remove_file(store.paths_for(&Workspace::Personal, "note-id").baseline)
            .expect("baseline should be removable");

        let error = store
            .load_for_local_path(&local_path)
            .expect_err("a missing baseline must prevent sync");
        assert!(matches!(&error, StateError::MissingBaseline { .. }));
        let message = error.to_string();
        assert!(message.contains("re-pull note note-id from the personal workspace"));

        // Named as the record keeps it: canonical, which on Windows is the
        // verbatim `\\?\` form rather than the temp path as spelled.
        assert!(message.contains(&state.local_path.display().to_string()));
    }

    #[test]
    fn malformed_index_keys_and_unrelated_sidecars_do_not_block_scan_recovery() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let state = TrackedNoteState::capture(
            "note/id".to_owned(),
            Workspace::Personal,
            local_path.clone(),
            "baseline",
            Some(1),
        )
        .expect("fixture state should capture");
        store
            .persist_from_sync(&state, "baseline")
            .expect("state should persist");

        let canonical = fs::canonicalize(&local_path).expect("path should canonicalize");
        fs::write(store.index_path(&canonical), "../../outside")
            .expect("malformed hint should write");
        fs::write(store.root().join("tracked/broken.json"), b"not json")
            .expect("unrelated corrupt sidecar should write");

        let loaded = store
            .load_for_local_path(&local_path)
            .expect("scan should ignore both corrupt hints");
        assert_eq!(loaded.state.internal_id, "note/id");
    }

    #[test]
    fn corrupt_indexed_sidecar_has_a_focused_error_after_fallback_scan() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let state = TrackedNoteState::capture(
            "note-id".to_owned(),
            Workspace::Personal,
            local_path.clone(),
            "baseline",
            Some(1),
        )
        .expect("fixture state should capture");
        store
            .persist_from_sync(&state, "baseline")
            .expect("state should persist");
        fs::write(
            store.paths_for(&Workspace::Personal, "note-id").sidecar,
            b"not json",
        )
        .expect("sidecar should corrupt");

        assert!(matches!(
            store.load_for_local_path(&local_path),
            Err(StateError::CorruptTrackedState { local_path: path, .. }) if path == fs::canonicalize(local_path).expect("path should canonicalize")
        ));
    }

    #[test]
    fn corrupt_indexed_sidecar_falls_back_to_another_valid_record() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");

        store
            .persist_from_sync(
                &capture("fallback", Workspace::Personal, &local_path),
                "baseline",
            )
            .expect("state should persist");
        // The pointer names a sidecar that is now garbage.
        let indexed = store.paths_for(&Workspace::Personal, "indexed").sidecar;
        fs::write(&indexed, b"not json").expect("indexed sidecar should corrupt");
        fs::write(
            store.index_path(&fs::canonicalize(&local_path).expect("path canonicalizes")),
            "personal--indexed",
        )
        .expect("pointer should redirect");

        let loaded = store
            .load_for_local_path(&local_path)
            .expect("scan should recover the valid alternative");
        assert_eq!(loaded.state.internal_id, "fallback");
    }

    #[cfg(unix)]
    #[test]
    fn state_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temp directory should create");
        let root = directory.path().join("state");
        let store = StateStore::new(root.clone());
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        store
            .persist_from_sync(
                &TrackedNoteState::capture(
                    "id".to_owned(),
                    Workspace::Personal,
                    local_path,
                    "baseline",
                    Some(1),
                )
                .expect("fixture state should capture"),
                "baseline",
            )
            .expect("state should persist");

        let mode = fs::metadata(root.join("tracked"))
            .expect("tracked directory should exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// Anyone could have planted a sidecar in a world-writable directory, so
    /// nothing is read from one.
    #[cfg(unix)]
    #[test]
    fn a_world_writable_state_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temp directory should create");
        let root = directory.path().join("state");
        fs::create_dir(&root).expect("state directory should create");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o777))
            .expect("permissions should set");
        let store = StateStore::new(root.clone());
        assert!(matches!(
            store.list_tracked(),
            Err(StateError::UntrustedStateDir {
                reason: "is writable by other accounts, or holds a directory that is; chmod -R go-w it",
                ..
            })
        ));

        // A private root does not vouch for an open directory inside it.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .expect("permissions should set");
        fs::create_dir(root.join("tracked")).expect("tracked directory should create");
        fs::set_permissions(root.join("tracked"), fs::Permissions::from_mode(0o777))
            .expect("permissions should set");
        assert!(matches!(
            store.list_tracked(),
            Err(StateError::UntrustedStateDir { .. })
        ));
    }

    /// Kind names are a contract, so every variant's kind is pinned here.
    #[test]
    fn every_variant_keeps_its_kind() {
        use crate::reply::{ErrorKind, ToolError};

        let path = || PathBuf::from("/x");
        let cases = [
            (StateError::NotTracked, ErrorKind::NotTracked),
            (StateError::InvalidStatePath, ErrorKind::LocalIo),
            (
                StateError::Io(std::io::Error::other("x")),
                ErrorKind::LocalIo,
            ),
            (
                StateError::Serialize(serde_json::from_str::<u8>("x").expect_err("not JSON")),
                ErrorKind::LocalIo,
            ),
            (
                StateError::UntrustedStateDir {
                    path: path(),
                    reason: "r",
                },
                ErrorKind::LocalAccess,
            ),
            (
                StateError::CorruptTrackedState {
                    sidecar_path: path(),
                    local_path: path(),
                },
                ErrorKind::SyncState,
            ),
            (StateError::StateIdentityMismatch, ErrorKind::SyncState),
            (
                StateError::AmbiguousTrackedState { local_path: path() },
                ErrorKind::SyncState,
            ),
            (
                StateError::MissingBaseline {
                    baseline_path: path(),
                    note_id: "n".to_owned(),
                    workspace: Workspace::Personal,
                    local_path: path(),
                },
                ErrorKind::SyncState,
            ),
            (
                StateError::BaselineMismatch {
                    baseline_path: path(),
                    note_id: "n".to_owned(),
                    workspace: Workspace::Personal,
                    local_path: path(),
                },
                ErrorKind::SyncState,
            ),
        ];
        for (error, kind) in cases {
            assert_eq!(error.kind(), kind, "{error}");
        }
    }

    #[test]
    fn constructing_store_does_not_create_state() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let root = directory.path().join("state");
        let store = StateStore::new(root.clone());

        assert_eq!(store.root(), root);
        assert!(!root.exists());
    }

    #[test]
    fn paths_encode_note_ids_and_distinguish_workspaces() {
        let store = StateStore::new(PathBuf::from("/state"));
        let personal = store.paths_for(&Workspace::Personal, "note/id");
        let team = store.paths_for(
            &Workspace::Team {
                team_path: "team/path".to_owned(),
            },
            "note/id",
        );

        assert!(personal.sidecar.ends_with("personal--note%2Fid.json"));
        assert!(team.sidecar.ends_with("team--team%2Fpath--note%2Fid.json"));
        assert_ne!(personal.sidecar, team.sidecar);
    }

    #[test]
    fn sync_persistence_writes_one_sidecar_and_exact_private_baseline() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let store = StateStore::new(directory.path().join("state"));
        let state = fixture_state(Workspace::Personal);
        store
            .persist_from_sync(&state, "# Exact baseline\n")
            .expect("state should persist");
        let paths = store.paths_for(&state.workspace, &state.internal_id);

        assert_eq!(
            fs::read_to_string(&paths.baseline).expect("baseline should be readable"),
            "# Exact baseline\n"
        );
        let decoded: TrackedNoteState =
            serde_json::from_slice(&fs::read(&paths.sidecar).expect("sidecar should be readable"))
                .expect("sidecar should deserialize");
        assert_eq!(decoded, state);
        let sidecar_text = fs::read_to_string(paths.sidecar).expect("sidecar should be text");
        assert!(!sidecar_text.to_ascii_lowercase().contains("token"));
        assert_eq!(
            fs::read_dir(store.root().join("tracked"))
                .expect("tracked directory should be readable")
                .count(),
            2,
            "one sidecar and one baseline should remain after atomic writes"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(paths.baseline)
                    .expect("baseline metadata should exist")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn untracking_an_old_note_does_not_remove_a_reused_paths_new_hint() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("working Markdown should write");

        for note_id in ["old", "new"] {
            let state = TrackedNoteState::capture(
                note_id.to_owned(),
                Workspace::Personal,
                local_path.clone(),
                "baseline",
                Some(1),
            )
            .expect("tracked state should capture");
            store
                .persist_from_sync(&state, "baseline")
                .expect("tracked state should persist");
        }

        // Pulling "new" onto the file displaced "old", so there is nothing left
        // to untrack, and the file still belongs to "new".
        assert!(matches!(
            store.untrack(&Workspace::Personal, "old"),
            Err(StateError::NotTracked)
        ));
        let loaded = store
            .load_for_local_path(&local_path)
            .expect("new note hint should remain usable");
        assert_eq!(loaded.state.internal_id, "new");
    }

    fn capture(note_id: &str, workspace: Workspace, local_path: &Path) -> TrackedNoteState {
        TrackedNoteState::capture(
            note_id.to_owned(),
            workspace,
            local_path,
            "baseline",
            Some(1),
        )
        .expect("tracked state should capture")
    }

    #[test]
    fn a_note_pulled_over_another_leaves_no_record_that_could_claim_the_file() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("working Markdown should write");

        for note_id in ["a", "b"] {
            store
                .persist_from_sync(
                    &capture(note_id, Workspace::Personal, &local_path),
                    "baseline",
                )
                .expect("tracked state should persist");
        }
        assert_eq!(
            store
                .list_tracked()
                .expect("records should list")
                .iter()
                .map(|state| state.internal_id.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );

        // Untracking "b" drops the pointer too. Before, the scan then found
        // "a", and a push of this file would have written "b"'s body to "a".
        store
            .untrack(&Workspace::Personal, "b")
            .expect("b should untrack");
        assert!(matches!(
            store.load_for_local_path(&local_path),
            Err(StateError::NotTracked)
        ));
    }

    #[test]
    fn two_records_naming_one_file_are_refused_not_picked_by_scan_order() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("working Markdown should write");
        store
            .persist_from_sync(&capture("a", Workspace::Personal, &local_path), "baseline")
            .expect("tracked state should persist");

        // A second record for the same file, as state from an older build or an
        // interrupted pull can leave behind.
        let tracked = store.root().join("tracked");
        fs::copy(
            tracked.join("personal--a.json"),
            tracked.join("personal--other.json"),
        )
        .expect("sidecar should copy");
        fs::remove_dir_all(store.root().join("by-path")).expect("index should be removable");

        let error = store
            .load_for_local_path(&local_path)
            .expect_err("two records for one file must not resolve");
        assert!(matches!(error, StateError::AmbiguousTrackedState { .. }));
        assert!(
            error
                .to_string()
                .starts_with("more than one tracked note records ")
        );
    }

    #[test]
    fn a_corrupt_record_is_repaired_by_persisting_over_it() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("working Markdown should write");
        let state = capture("a", Workspace::Personal, &local_path);
        store
            .persist_from_sync(&state, "baseline")
            .expect("tracked state should persist");
        fs::write(store.root().join("tracked/personal--a.json"), b"not json")
            .expect("sidecar should corrupt");

        store
            .persist_from_sync(&state, "baseline")
            .expect("a re-pull must be able to replace a corrupt record");
        assert_eq!(
            store
                .load_for_local_path(&local_path)
                .expect("record loads")
                .state,
            state
        );
    }

    #[test]
    fn a_migration_interrupted_after_the_new_write_is_finished_on_the_next_sync() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("working Markdown should write");
        let state = capture("note-id", Workspace::Personal, &local_path);
        store
            .persist_from_sync(&state, "baseline")
            .expect("state should persist under the new key");
        // The legacy pair a crash left behind before it could be removed.
        let tracked = store.root().join("tracked");
        fs::copy(
            tracked.join("personal--note%2Did.json"),
            tracked.join("personal--note-id.json"),
        )
        .expect("legacy sidecar should copy");
        fs::write(tracked.join("personal--note-id.baseline.md"), "baseline")
            .expect("legacy baseline should write");

        store
            .persist_from_sync(&state, "baseline")
            .expect("the next sync should persist");
        fs::remove_dir_all(store.root().join("by-path")).expect("index should be removable");
        assert_eq!(
            store
                .load_for_local_path(&local_path)
                .expect("the scan should find one record")
                .state,
            state
        );
    }

    #[test]
    fn hyphens_cannot_make_two_notes_share_a_key() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let first = directory.path().join("first.md");
        let second = directory.path().join("second.md");
        fs::write(&first, "baseline").expect("working Markdown should write");
        fs::write(&second, "baseline").expect("working Markdown should write");
        let team = |team_path: &str| Workspace::Team {
            team_path: team_path.to_owned(),
        };

        store
            .persist_from_sync(&capture("c", team("a--b"), &first), "baseline")
            .expect("first note should persist");
        store
            .persist_from_sync(&capture("b--c", team("a"), &second), "baseline")
            .expect("second note should persist");

        assert_eq!(
            store
                .load_for_local_path(&first)
                .expect("first loads")
                .state
                .internal_id,
            "c"
        );
        assert_eq!(
            store
                .load_for_local_path(&second)
                .expect("second loads")
                .state
                .internal_id,
            "b--c"
        );
        assert_eq!(store.list_tracked().expect("records should list").len(), 2);
    }

    #[test]
    fn records_under_the_older_hyphen_key_load_and_move_on_the_next_sync() {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let store = StateStore::new(directory.path().join("state"));
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("working Markdown should write");
        let state = capture("note-id", Workspace::Personal, &local_path);
        // What an older build wrote: `-` left bare in the key.
        let tracked = store.root().join("tracked");
        fs::create_dir_all(&tracked).expect("tracked directory should create");
        fs::write(
            tracked.join("personal--note-id.json"),
            serde_json::to_vec(&state).expect("state should serialize"),
        )
        .expect("legacy sidecar should write");
        fs::write(tracked.join("personal--note-id.baseline.md"), "baseline")
            .expect("legacy baseline should write");

        assert_eq!(
            store
                .load_for_local_path(&local_path)
                .expect("legacy record loads")
                .state,
            state
        );
        let mut with_snapshot = state.clone();
        with_snapshot.remote_snapshot_hash = Some("sha256:snapshot".to_owned());
        store
            .update_sidecar(&with_snapshot)
            .expect("legacy sidecar should update in place");
        assert!(!tracked.join("personal--note%2Did.json").exists());

        store
            .persist_from_sync(&state, "baseline")
            .expect("state should persist under the new key");
        let mut names: Vec<_> = fs::read_dir(&tracked)
            .expect("tracked directory should list")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("utf-8")
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "personal--note%2Did.baseline.md",
                "personal--note%2Did.json"
            ]
        );
        let loaded = store
            .load_for_local_path(&local_path)
            .expect("record loads");
        assert_eq!(
            loaded.state.remote_snapshot_hash.as_deref(),
            Some("sha256:snapshot")
        );
        assert!(store.untrack(&Workspace::Personal, "note-id").is_ok());
    }
}
