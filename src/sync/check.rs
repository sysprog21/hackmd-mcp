use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    local::{LocalAccessError, LocalFiles},
    sync::state::{StateError, body_digest, body_hash_from_digest, timestamp_text},
    sync::{ChangeState, LocalBodyError, classify_changes, read_local_body},
};

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
    #[error(transparent)]
    LocalBody(#[from] LocalBodyError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

impl crate::reply::ToolError for CheckNoteSyncError {
    fn kind(&self) -> crate::reply::ErrorKind {
        match self {
            Self::Access(error) => error.kind(),
            Self::LocalBody(error) => error.kind(),
            Self::State(error) => error.kind(),
            Self::Api(error) => error.kind(),
        }
    }
}

pub(crate) async fn check_note_sync(
    client: &HackmdClient,
    files: &LocalFiles,
    local_path: &Path,
) -> Result<CheckNoteSyncOutput, CheckNoteSyncError> {
    files.allow(local_path)?;
    // The record first: an untracked path fails before its file is read.
    let tracked = files.state().load_for_local_path(local_path)?;
    // Only looking needs no large-file confirmation, but the maximum holds.
    let local = read_local_body(files, local_path, true)?;
    let (remote_note, remote) = client
        .get_note_body(&tracked.state.workspace, &tracked.state.internal_id)
        .await?;
    let (local_digest, remote_digest) =
        crate::local::offload(|| (body_digest(&local), body_digest(&remote)));

    // Both sides changed to the same body, as a push whose write landed but
    // whose state was not saved leaves them: in sync, as push agrees, and the
    // next push moves the baseline there.
    let status = match classify_changes(&tracked.baseline_digest, &local_digest, &remote_digest) {
        ChangeState::Conflict if local_digest == remote_digest => SyncStatus::InSync,
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

    use super::{SyncStatus, check_note_sync};
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
            // Both moved off the baseline, to the same body.
            ("remote", SyncStatus::InSync),
        ] {
            fs::write(&local_path, local).expect("local fixture should update");
            let output = check_note_sync(&client, &files, &local_path)
                .await
                .expect("check should succeed");
            assert_eq!(output.status, expected);
            assert!(output.remote_body_hash.starts_with("sha256:"));
            statuses.push(output.status);
        }
        assert_eq!(statuses.len(), 5);
        fixture.finish();
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should remain readable"),
            "remote"
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

        let output = check_note_sync(&fixture.client(), &files, &local_path)
            .await
            .expect("timestamp-only check should succeed");
        assert_eq!(output.status, SyncStatus::InSync);
        assert_eq!(output.remote_timestamp, "99");
        fixture.finish();
    }
}
