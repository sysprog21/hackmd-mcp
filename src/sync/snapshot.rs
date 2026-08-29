use std::path::PathBuf;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    local::{LocalAccessError, LocalFiles},
    sync::state::{StateError, write_local_atomic},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SaveRemoteSnapshotInput {
    pub(crate) local_path: PathBuf,
    #[serde(default)]
    pub(crate) overwrite_snapshot: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct SaveRemoteSnapshotOutput {
    pub(crate) note_id: String,
    pub(crate) local_path: PathBuf,
    pub(crate) snapshot_path: PathBuf,
    pub(crate) bytes: usize,
}

#[derive(Debug, Error)]
pub(crate) enum SaveRemoteSnapshotError {
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error("local_path must be absolute")]
    RelativePath,
    #[error("remote snapshot already exists; retry with overwrite_snapshot: true")]
    SnapshotExists,
    #[error("remote note {note_id} has no Markdown content")]
    MissingRemoteContent { note_id: String },
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn save_remote_snapshot(
    client: &HackmdClient,
    files: &LocalFiles,
    input: SaveRemoteSnapshotInput,
) -> Result<SaveRemoteSnapshotOutput, SaveRemoteSnapshotError> {
    if !input.local_path.is_absolute() {
        return Err(SaveRemoteSnapshotError::RelativePath);
    }
    files.allow(&input.local_path)?;
    let tracked = files.state().load_for_local_path(&input.local_path)?;

    // The write target is derived from the tracked state, not from the checked
    // input, so it goes through the same policy before anything is created.
    let snapshot_path = tracked.state.local_path.with_extension("remote.md");
    files.allow(&snapshot_path)?;
    if snapshot_path.exists() && !input.overwrite_snapshot {
        return Err(SaveRemoteSnapshotError::SnapshotExists);
    }
    let remote = client
        .get_note(&tracked.state.workspace, &tracked.state.internal_id)
        .await?
        .content
        .ok_or_else(|| SaveRemoteSnapshotError::MissingRemoteContent {
            note_id: tracked.state.internal_id.clone(),
        })?;
    write_local_atomic(&snapshot_path, remote.as_bytes())?;
    Ok(SaveRemoteSnapshotOutput {
        note_id: tracked.state.internal_id,
        local_path: tracked.state.local_path,
        snapshot_path,
        bytes: remote.len(),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{SaveRemoteSnapshotError, SaveRemoteSnapshotInput, save_remote_snapshot};
    #[tokio::test]
    async fn snapshot_is_atomic_separate_and_requires_overwrite_confirmation() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local working").expect("local fixture should write");
        let fixture = crate::fixture::SequenceServer::spawn([
            (200, r#"{"id":"id","title":"Note","content":"remote one"}"#),
            (200, r#"{"id":"id","title":"Note","content":"remote two"}"#),
        ]);
        let client = fixture.client();
        let files = crate::fixture::tracked_files(directory.path(), "id", &local_path, "baseline");

        let output = save_remote_snapshot(
            &client,
            &files,
            SaveRemoteSnapshotInput {
                local_path: local_path.clone(),
                overwrite_snapshot: false,
            },
        )
        .await
        .expect("snapshot should save");
        assert!(output.snapshot_path.ends_with("note.remote.md"));
        assert_eq!(
            fs::read_to_string(&output.snapshot_path).expect("snapshot should read"),
            "remote one"
        );
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should read"),
            "local working"
        );
        assert!(matches!(
            save_remote_snapshot(
                &client,
                &files,
                SaveRemoteSnapshotInput {
                    local_path: local_path.clone(),
                    overwrite_snapshot: false,
                }
            )
            .await,
            Err(SaveRemoteSnapshotError::SnapshotExists)
        ));
        save_remote_snapshot(
            &client,
            &files,
            SaveRemoteSnapshotInput {
                local_path: local_path.clone(),
                overwrite_snapshot: true,
            },
        )
        .await
        .expect("confirmed overwrite should save");
        assert_eq!(
            fs::read_to_string(output.snapshot_path).expect("snapshot should read"),
            "remote two"
        );
        assert_eq!(fixture.finish().len(), 2);
    }
}
