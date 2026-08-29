use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
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

pub(crate) struct LoadedTrackedState {
    pub(crate) state: TrackedNoteState,
    pub(crate) baseline_body: String,
    pub(crate) baseline_path: PathBuf,
}

impl StateStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[cfg(test)]
    fn root(&self) -> &Path {
        &self.root
    }

    fn paths_for(&self, workspace: &Workspace, internal_id: &str) -> StatePaths {
        let key = match workspace {
            Workspace::Personal => format!("personal--{}", encode_component(internal_id)),
            Workspace::Team { team_path } => format!(
                "team--{}--{}",
                encode_component(team_path),
                encode_component(internal_id)
            ),
        };
        let tracked = self.root.join("tracked");
        StatePaths {
            sidecar: tracked.join(format!("{key}.json")),
            baseline: tracked.join(format!("{key}.baseline.md")),
        }
    }

    /// Persists sync state atomically per file. Only pull/push handlers should
    /// call this operation; server startup constructs the store without writes.
    #[allow(dead_code, reason = "called by the local pull/push tool tasks")]
    pub(crate) fn persist_from_sync(
        &self,
        state: &TrackedNoteState,
        baseline_body: &str,
    ) -> Result<(), StateError> {
        let paths = self.paths_for(&state.workspace, &state.internal_id);
        let parent = paths.sidecar.parent().ok_or(StateError::InvalidStatePath)?;
        fs::create_dir_all(parent)?;
        write_private_atomic(&paths.baseline, baseline_body.as_bytes())?;
        let sidecar = serde_json::to_vec_pretty(state)?;
        write_private_atomic(&paths.sidecar, &sidecar)?;
        Ok(())
    }

    pub(crate) fn load_for_local_path(
        &self,
        local_path: &Path,
    ) -> Result<LoadedTrackedState, StateError> {
        let canonical = fs::canonicalize(local_path)?;
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
            let state: TrackedNoteState = serde_json::from_slice(&fs::read(&path)?)?;
            if state.local_file_identity.canonical_path == canonical {
                let paths = self.paths_for(&state.workspace, &state.internal_id);
                let baseline_body = fs::read_to_string(&paths.baseline)?;
                return Ok(LoadedTrackedState {
                    state,
                    baseline_body,
                    baseline_path: paths.baseline,
                });
            }
        }
        Err(StateError::NotTracked)
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

fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<(), StateError> {
    let parent = path.parent().ok_or(StateError::InvalidStatePath)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    set_private_permissions(temporary.as_file())?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| StateError::Io(error.error))?;
    Ok(())
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
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{LocalFileIdentity, StateStore, TrackedNoteState};
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
}
