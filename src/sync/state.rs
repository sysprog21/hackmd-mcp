use std::{
    fmt::Write as _,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::models::Workspace;

/// Persistent metadata for one locally tracked note.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct TrackedNoteState {
    pub(crate) internal_id: String,
    pub(crate) workspace: Workspace,
    pub(crate) local_path: PathBuf,
    pub(crate) baseline_body_hash: String,
    pub(crate) last_observed_remote_timestamp: String,
    pub(crate) local_file_identity: LocalFileIdentity,
}

impl TrackedNoteState {
    /// Records the note as tracked at the moment `baseline_body` is what both
    /// `local_path` and `HackMD` hold. Every sync tool goes through here so the
    /// stored hash and file identity can never disagree with the baseline.
    pub(crate) fn capture(
        internal_id: String,
        workspace: Workspace,
        local_path: PathBuf,
        baseline_body: &str,
        remote_timestamp: Option<i64>,
    ) -> Result<Self, StateError> {
        Ok(Self {
            local_file_identity: local_file_identity(&local_path)?,
            internal_id,
            workspace,
            local_path,
            baseline_body_hash: body_hash(baseline_body),
            last_observed_remote_timestamp: timestamp_text(remote_timestamp),
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
            self.local_path,
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
    match workspace {
        Workspace::Personal => format!("personal--{}", encode_component(internal_id)),
        Workspace::Team { team_path } => format!(
            "team--{}--{}",
            encode_component(team_path),
            encode_component(internal_id)
        ),
    }
}

/// The single hash format written to sidecars and reported by the sync tools.
pub(crate) fn body_hash(body: &str) -> String {
    body_hash_from_digest(&body_digest(body))
}

pub(crate) fn body_hash_from_digest(digest: &[u8; 32]) -> String {
    let mut hash = String::with_capacity(71);
    hash.push_str("sha256:");
    for byte in digest {
        write!(hash, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hash
}

pub(crate) fn body_digest(body: &str) -> [u8; 32] {
    Sha256::digest(body.as_bytes()).into()
}

/// Stable identity captured for the local Markdown file.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct LocalFileIdentity {
    pub(crate) canonical_path: PathBuf,
    pub(crate) device_id: Option<u64>,
    pub(crate) file_id: Option<u64>,
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

    pub(crate) fn probe_writable(&self) -> Result<(), StateError> {
        create_private_dir_all(&self.root)?;
        let temporary = NamedTempFile::new_in(&self.root)?;
        set_private_permissions(temporary.as_file())?;
        temporary.as_file().sync_all()?;
        Ok(())
    }

    #[cfg(test)]
    fn root(&self) -> &Path {
        &self.root
    }

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
        self.root.join("by-path").join(format!("{digest:x}"))
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
        let key = state_key(&state.workspace, &state.internal_id);
        let paths = self.paths_for_key(&key);
        if let Ok(previous) = fs::read(&paths.sidecar).and_then(|bytes| {
            serde_json::from_slice::<TrackedNoteState>(&bytes).map_err(io::Error::other)
        }) && previous.local_file_identity.canonical_path
            != state.local_file_identity.canonical_path
        {
            self.remove_hint_if_matches(&previous.local_file_identity.canonical_path, &key)?;
        }
        write_private_atomic(&paths.baseline, baseline_body.as_bytes())?;
        let sidecar = serde_json::to_vec_pretty(state)?;
        write_private_atomic(&paths.sidecar, &sidecar)?;

        // A hint, not a source of truth: the loader verifies what it finds and
        // falls back to a scan, so a stale or missing pointer costs speed and
        // never correctness. That is also why it is written without the fsync
        // the sidecar pair gets.
        write_private_hint(
            &self.index_path(&state.local_file_identity.canonical_path),
            key.as_bytes(),
        )?;
        Ok(())
    }

    /// Reads every tracked sidecar without touching the working Markdown or
    /// baseline files. Callers use this for state discovery, not sync safety;
    /// an individual sync still verifies its baseline before comparison.
    pub(crate) fn list_tracked(&self) -> Result<Vec<TrackedNoteState>, StateError> {
        let tracked = self.root.join("tracked");
        let entries = match fs::read_dir(tracked) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(StateError::Io(error)),
        };
        let mut states = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                let bytes = fs::read(&path)?;
                match serde_json::from_slice(&bytes) {
                    Ok(state) => states.push(state),
                    Err(_) => {
                        tracing::warn!(path = %path.display(), "skipping malformed tracked sidecar");
                    }
                }
            }
        }
        Ok(states)
    }

    /// Stops tracking exactly one workspace/note pair. The working Markdown
    /// path in the sidecar is used only to locate its rebuildable index hint;
    /// the working file itself is never opened, changed, or removed.
    pub(crate) fn untrack(
        &self,
        workspace: &Workspace,
        internal_id: &str,
    ) -> Result<TrackedNoteState, StateError> {
        let paths = self.paths_for(workspace, internal_id);
        let sidecar = match fs::read(&paths.sidecar) {
            Ok(sidecar) => sidecar,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(StateError::NotTracked);
            }
            Err(error) => return Err(StateError::Io(error)),
        };
        let state: TrackedNoteState = serde_json::from_slice(&sidecar)?;
        if state.workspace != *workspace || state.internal_id != internal_id {
            return Err(StateError::StateIdentityMismatch);
        }

        self.remove_hint_if_matches(
            &state.local_file_identity.canonical_path,
            &state_key(workspace, internal_id),
        )?;
        remove_if_present(&paths.baseline)?;
        remove_if_present(&paths.sidecar)?;
        Ok(state)
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
    fn load_via_index(&self, canonical: &Path) -> Result<Option<LoadedTrackedState>, StateError> {
        let Ok(key) = fs::read_to_string(self.index_path(canonical)) else {
            return Ok(None);
        };
        let key = key.trim();
        if !valid_state_key(key) {
            return Ok(None);
        }
        let paths = self.paths_for_key(key);
        let sidecar = match fs::read(&paths.sidecar) {
            Ok(sidecar) => sidecar,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(StateError::CorruptTrackedState {
                    sidecar_path: paths.sidecar,
                    local_path: canonical.to_path_buf(),
                });
            }
        };
        let state: TrackedNoteState =
            serde_json::from_slice(&sidecar).map_err(|_| StateError::CorruptTrackedState {
                sidecar_path: paths.sidecar.clone(),
                local_path: canonical.to_path_buf(),
            })?;
        if state.local_file_identity.canonical_path != canonical {
            return Ok(None);
        }
        Self::load_verified(state, paths).map(Some)
    }

    /// Finds the tracked note whose recorded file is `local_path`.
    ///
    /// The by-path pointer answers this in one read; failing that, the sidecars
    /// are scanned and their canonical paths compared. Neither derives a
    /// filename from the path, so a note stays tracked when the caller reaches
    /// it through a symlink or a differently spelled path.
    pub(crate) fn load_for_local_path(
        &self,
        local_path: &Path,
    ) -> Result<LoadedTrackedState, StateError> {
        let canonical = fs::canonicalize(local_path)?;
        let indexed_error = match self.load_via_index(&canonical) {
            Ok(Some(loaded)) => return Ok(loaded),
            Ok(None) => None,
            Err(error) => Some(error),
        };
        let tracked = self.root.join("tracked");
        let entries = fs::read_dir(tracked).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                StateError::NotTracked
            } else {
                StateError::Io(error)
            }
        })?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)?;
            let state: TrackedNoteState = if let Ok(state) = serde_json::from_slice(&bytes) {
                state
            } else {
                tracing::warn!(path = %path.display(), "skipping malformed tracked sidecar");
                continue;
            };
            if state.local_file_identity.canonical_path == canonical {
                let paths = self.paths_for(&state.workspace, &state.internal_id);
                return Self::load_verified(state, paths);
            }
        }
        Err(indexed_error.unwrap_or(StateError::NotTracked))
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

pub(crate) fn write_local_atomic(path: &Path, contents: &[u8]) -> Result<(), StateError> {
    let parent = path.parent().ok_or(StateError::InvalidStatePath)?;
    let existing_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut temporary = NamedTempFile::new_in(parent)?;
    if let Some(permissions) = existing_permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| StateError::Io(error.error))?;
    Ok(())
}

pub(crate) fn local_file_identity(path: &Path) -> Result<LocalFileIdentity, StateError> {
    let canonical_path = fs::canonicalize(path)?;
    let metadata = fs::metadata(&canonical_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(LocalFileIdentity {
            canonical_path,
            device_id: Some(metadata.dev()),
            file_id: Some(metadata.ino()),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(LocalFileIdentity {
            canonical_path,
            device_id: None,
            file_id: None,
        })
    }
}

struct StatePaths {
    sidecar: PathBuf,
    baseline: PathBuf,
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
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
    let mut temporary = NamedTempFile::new_in(parent)?;
    set_private_permissions(temporary.as_file())?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| StateError::Io(error.error))?;
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
    #[error("invalid local state path")]
    InvalidStatePath,
    #[error("local state I/O failed")]
    Io(#[from] io::Error),
    #[error("local state serialization failed")]
    Serialize(#[from] serde_json::Error),
    #[error(
        "tracked sidecar {} for {} is corrupt; re-pull the note or untrack this record",
        sidecar_path.display(),
        local_path.display()
    )]
    CorruptTrackedState {
        sidecar_path: PathBuf,
        local_path: PathBuf,
    },
    #[error("tracked sidecar identity does not match its filename")]
    StateIdentityMismatch,
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

fn remove_if_present(path: &Path) -> Result<(), StateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{LocalFileIdentity, StateError, StateStore, TrackedNoteState};
    use crate::models::Workspace;

    fn fixture_state(workspace: Workspace) -> TrackedNoteState {
        TrackedNoteState {
            internal_id: "note/id".to_owned(),
            workspace,
            local_path: PathBuf::from("/tmp/note.md"),
            baseline_body_hash: "sha256:fixture".to_owned(),
            last_observed_remote_timestamp: "2026-08-29T00:00:00Z".to_owned(),
            local_file_identity: LocalFileIdentity {
                canonical_path: PathBuf::from("/tmp/note.md"),
                device_id: Some(1),
                file_id: Some(2),
            },
        }
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
        assert!(message.contains(&local_path.display().to_string()));
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

        for note_id in ["fallback", "indexed"] {
            let state = TrackedNoteState::capture(
                note_id.to_owned(),
                Workspace::Personal,
                local_path.clone(),
                "baseline",
                Some(1),
            )
            .expect("fixture state should capture");
            store
                .persist_from_sync(&state, "baseline")
                .expect("state should persist");
        }
        fs::write(
            store.paths_for(&Workspace::Personal, "indexed").sidecar,
            b"not json",
        )
        .expect("indexed sidecar should corrupt");

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

        store
            .untrack(&Workspace::Personal, "old")
            .expect("old note should untrack");
        let loaded = store
            .load_for_local_path(&local_path)
            .expect("new note hint should remain usable");
        assert_eq!(loaded.state.internal_id, "new");
    }
}
