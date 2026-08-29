use std::path::PathBuf;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    state::{StateError, write_local_atomic},
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
    input: SaveRemoteSnapshotInput,
) -> Result<SaveRemoteSnapshotOutput, SaveRemoteSnapshotError> {
    if !input.local_path.is_absolute() {
        return Err(SaveRemoteSnapshotError::RelativePath);
    }
    let tracked = client.state().load_for_local_path(&input.local_path)?;
    let snapshot_path = tracked.state.local_path.with_extension("remote.md");
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
    use crate::{
        client::HackmdClient,
        config::Config,
        models::Workspace,
        state::{TrackedNoteState, local_file_identity},
    };

    #[tokio::test]
    async fn snapshot_is_atomic_separate_and_requires_overwrite_confirmation() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local working").expect("local fixture should write");
        let fixture = crate::test_support::SequenceServer::spawn([
            (200, r#"{"id":"id","title":"Note","content":"remote one"}"#),
            (200, r#"{"id":"id","title":"Note","content":"remote two"}"#),
        ]);
        let client = HackmdClient::new(Config::for_loopback_test_with_state(
            &fixture.api_url,
            "fixture-token",
            &directory.path().join("state"),
        ))
        .expect("fixture client should build");
        client
            .state()
            .persist_from_sync(
                &TrackedNoteState {
                    internal_id: "id".to_owned(),
                    workspace: Workspace::Personal,
                    local_path: local_path.clone(),
                    baseline_body_hash: "sha256:fixture".to_owned(),
                    last_observed_remote_timestamp: "1".to_owned(),
                    local_file_identity: local_file_identity(&local_path)
                        .expect("identity should resolve"),
                },
                "baseline",
            )
            .expect("state should persist");

        let output = save_remote_snapshot(
            &client,
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
