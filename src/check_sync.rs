use std::{fs, path::PathBuf};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    state::StateError,
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
    input: CheckNoteSyncInput,
) -> Result<CheckNoteSyncOutput, CheckNoteSyncError> {
    if !input.local_path.is_absolute() {
        return Err(CheckNoteSyncError::RelativePath);
    }
    let local = fs::read_to_string(&input.local_path).map_err(|_| CheckNoteSyncError::LocalRead)?;
    let tracked = client.state().load_for_local_path(&input.local_path)?;
    let remote_note = client
        .get_note(&tracked.state.workspace, &tracked.state.internal_id)
        .await?;
    let remote = remote_note
        .content
        .ok_or_else(|| CheckNoteSyncError::MissingRemoteContent {
            note_id: tracked.state.internal_id.clone(),
        })?;
    let local_changed = local != tracked.baseline_body;
    let remote_changed = remote != tracked.baseline_body;
    let status = match (local_changed, remote_changed) {
        (false, false) => SyncStatus::InSync,
        (false, true) => SyncStatus::RemoteChanged,
        (true, false) => SyncStatus::LocalChanged,
        (true, true) => SyncStatus::Conflict,
    };
    Ok(CheckNoteSyncOutput {
        status,
        local_path: tracked.state.local_path,
        baseline_path: tracked.baseline_path,
        note_id: tracked.state.internal_id,
        remote_timestamp: remote_note
            .last_changed_at
            .map_or_else(String::new, |timestamp| timestamp.to_string()),
        baseline_body_hash: hash(&tracked.baseline_body),
        local_body_hash: hash(&local),
        remote_body_hash: hash(&remote),
    })
}

fn hash(body: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(body.as_bytes()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{CheckNoteSyncInput, SyncStatus, check_note_sync};
    use crate::{
        client::HackmdClient,
        config::Config,
        models::Workspace,
        state::{TrackedNoteState, local_file_identity},
    };

    #[tokio::test]
    async fn classifies_all_four_sync_states_without_writes() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let fixture = crate::test_support::SequenceServer::spawn([
            (
                200,
                r#"{"id":"id","title":"Note","content":"baseline","lastChangedAt":1}"#,
            ),
            (
                200,
                r#"{"id":"id","title":"Note","content":"remote","lastChangedAt":2}"#,
            ),
            (
                200,
                r#"{"id":"id","title":"Note","content":"baseline","lastChangedAt":1}"#,
            ),
            (
                200,
                r#"{"id":"id","title":"Note","content":"remote","lastChangedAt":2}"#,
            ),
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
        assert_eq!(fixture.finish().len(), 4);
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should remain readable"),
            "local"
        );
    }
}
