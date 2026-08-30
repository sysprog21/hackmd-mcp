use std::path::PathBuf;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    local::{LocalAccessError, LocalFiles},
    sync::state::{StateError, body_digest, body_hash_from_digest, timestamp_text},
    sync::{ChangeState, classify_changes},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CheckNoteSyncInput {
    pub(crate) local_path: PathBuf,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncStatus {
    InSync,
    RemoteChanged,
    LocalChanged,
    Conflict,
}

#[derive(Debug, Serialize)]
pub(crate) struct CheckNoteSyncOutput {
    pub(crate) status: SyncStatus,
    pub(crate) local_path: PathBuf,
    pub(crate) baseline_path: PathBuf,
    pub(crate) note_id: String,
    pub(crate) remote_timestamp: String,
    pub(crate) baseline_body_hash: String,
    pub(crate) local_body_hash: String,
    pub(crate) remote_body_hash: String,
}

#[derive(Debug, Error)]
pub(crate) enum CheckNoteSyncError {
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error("local_path must be absolute")]
    RelativePath,
    #[error("local Markdown file could not be read")]
    LocalRead,
    #[error("remote note {note_id} has no Markdown content")]
    MissingRemoteContent { note_id: String },
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn check_note_sync(
    client: &HackmdClient,
    files: &LocalFiles,
    input: CheckNoteSyncInput,
) -> Result<CheckNoteSyncOutput, CheckNoteSyncError> {
    if !input.local_path.is_absolute() {
        return Err(CheckNoteSyncError::RelativePath);
    }
    files.allow(&input.local_path)?;
    let local = files
        .read_to_string(&input.local_path)
        .map_err(|_| CheckNoteSyncError::LocalRead)?;
    let tracked = files.state().load_for_local_path(&input.local_path)?;
    let remote_note = client
        .get_note(&tracked.state.workspace, &tracked.state.internal_id)
        .await?;
    let remote = remote_note
        .content
        .ok_or_else(|| CheckNoteSyncError::MissingRemoteContent {
            note_id: tracked.state.internal_id.clone(),
        })?;
    let local_digest = body_digest(&local);
    let remote_digest = body_digest(&remote);
    let status = match classify_changes(&tracked.baseline_digest, &local_digest, &remote_digest) {
        ChangeState::InSync => SyncStatus::InSync,
        ChangeState::LocalOnly => SyncStatus::LocalChanged,
        ChangeState::RemoteOnly => SyncStatus::RemoteChanged,
        ChangeState::Conflict => SyncStatus::Conflict,
    };
    Ok(CheckNoteSyncOutput {
        status,
        local_path: tracked.state.local_path,
        baseline_path: tracked.baseline_path,
        note_id: tracked.state.internal_id,
        remote_timestamp: timestamp_text(remote_note.last_changed_at),
        // The loader already verified this hash against the baseline file.
        baseline_body_hash: tracked.state.baseline_body_hash,
        local_body_hash: body_hash_from_digest(&local_digest),
        remote_body_hash: body_hash_from_digest(&remote_digest),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{CheckNoteSyncInput, SyncStatus, check_note_sync};
    use crate::fixture::{Scenario, SequenceServer};
    #[tokio::test]
    async fn classifies_all_four_sync_states_without_writes() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"Note","content":"baseline","lastChangedAt":1}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"Note","content":"remote","lastChangedAt":2}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"Note","content":"baseline","lastChangedAt":1}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"Note","content":"remote","lastChangedAt":2}"#,
            ),
        ]);
        let client = fixture.client();
        let files = crate::fixture::tracked_files(directory.path(), "id", &local_path, "baseline");

        let mut statuses = Vec::new();
        for (local, expected) in [
            ("baseline", SyncStatus::InSync),
            ("baseline", SyncStatus::RemoteChanged),
            ("local", SyncStatus::LocalChanged),
            ("local", SyncStatus::Conflict),
        ] {
            fs::write(&local_path, local).expect("local fixture should update");
            let output = check_note_sync(
                &client,
                &files,
                CheckNoteSyncInput {
                    local_path: local_path.clone(),
                },
            )
            .await
            .expect("check should succeed");
            assert_eq!(output.status, expected);
            assert!(output.remote_body_hash.starts_with("sha256:"));
            statuses.push(output.status);
        }
        assert_eq!(statuses.len(), 4);
        fixture.finish();
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should remain readable"),
            "local"
        );
    }

    #[tokio::test]
    async fn timestamp_only_change_remains_in_sync() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "same body").expect("local fixture should write");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/id",
            200,
            r#"{"id":"id","title":"Note","content":"same body","lastChangedAt":99}"#,
        )]);
        let files = crate::fixture::tracked_files(directory.path(), "id", &local_path, "same body");

        let output = check_note_sync(&fixture.client(), &files, CheckNoteSyncInput { local_path })
            .await
            .expect("timestamp-only check should succeed");
        assert_eq!(output.status, SyncStatus::InSync);
        assert_eq!(output.remote_timestamp, "99");
        fixture.finish();
    }
}
