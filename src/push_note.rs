use std::{fs, path::PathBuf};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::UpdateNoteRequest,
    models::Workspace,
    note_ref::{NoteRefError, NoteResolution},
    state::{StateError, TrackedNoteState, local_file_identity},
};

const WARNING_BYTES: usize = 5 * 1024 * 1024;
const MAX_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PushStrategy {
    Safe,
    Overwrite,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PushNoteInput {
    #[serde(default)]
    pub(crate) workspace: Workspace,
    pub(crate) note_ref: String,
    pub(crate) local_path: PathBuf,
    #[serde(default = "default_strategy")]
    #[schemars(default = "default_strategy")]
    pub(crate) strategy: PushStrategy,
    #[serde(default)]
    pub(crate) confirm: bool,
    #[serde(default)]
    pub(crate) confirm_large_file: bool,
}

const fn default_strategy() -> PushStrategy {
    PushStrategy::Safe
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PushStatus {
    Pushed,
    NothingToPush,
    RemoteChanged,
    Conflict,
}

#[derive(Debug, Serialize)]
pub(crate) struct PushNoteOutput {
    pub(crate) status: PushStatus,
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    pub(crate) local_path: PathBuf,
    pub(crate) baseline_path: PathBuf,
    pub(crate) pushed: bool,
    pub(crate) merge_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diff_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snapshot_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum PushNoteError {
    #[error("local_path must be absolute")]
    RelativePath,
    #[error("local_path must be a readable regular file")]
    InvalidLocalFile,
    #[error("overwrite strategy requires confirm: true")]
    OverwriteConfirmationRequired,
    #[error("local file is {size_bytes} bytes; retry with confirm_large_file: true")]
    LargeFileConfirmationRequired { size_bytes: usize },
    #[error("local file is {size_bytes} bytes; files above 50 MiB are refused")]
    TooLarge { size_bytes: usize },
    #[error("note_ref/workspace resolves to a different note than the local sync sidecar")]
    TrackingMismatch,
    #[error("remote note {note_id} has no Markdown content")]
    MissingRemoteContent { note_id: String },
    #[error("HackMD accepted the update for note {note_id}, but read-back content did not match")]
    ReadbackMismatch { note_id: String },
    #[error("local file I/O failed")]
    LocalIo(#[source] std::io::Error),
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn push_note(
    client: &HackmdClient,
    input: PushNoteInput,
) -> Result<Result<PushNoteOutput, NoteResolution>, PushNoteError> {
    let local = validate_and_read_local(&input)?;
    let tracked = client.state().load_for_local_path(&input.local_path)?;
    let resolution =
        crate::note_ref::resolve_note_ref(client, input.workspace.clone(), &input.note_ref).await?;
    let NoteResolution::Resolved { note } = resolution else {
        return Ok(Err(resolution));
    };
    if note.note_id != tracked.state.internal_id || note.workspace != tracked.state.workspace {
        return Err(PushNoteError::TrackingMismatch);
    }
    let remote_note = client.get_note(&note.workspace, &note.note_id).await?;
    let remote =
        remote_note
            .content
            .clone()
            .ok_or_else(|| PushNoteError::MissingRemoteContent {
                note_id: note.note_id.clone(),
            })?;
    push_resolved(client, input, tracked, note, remote_note, remote, local).await
}

fn validate_and_read_local(input: &PushNoteInput) -> Result<String, PushNoteError> {
    if !input.local_path.is_absolute() {
        return Err(PushNoteError::RelativePath);
    }
    if matches!(input.strategy, PushStrategy::Overwrite) && !input.confirm {
        return Err(PushNoteError::OverwriteConfirmationRequired);
    }
    let metadata = fs::metadata(&input.local_path).map_err(|_| PushNoteError::InvalidLocalFile)?;
    if !metadata.is_file() {
        return Err(PushNoteError::InvalidLocalFile);
    }
    let local = fs::read_to_string(&input.local_path).map_err(PushNoteError::LocalIo)?;
    if local.len() > MAX_BYTES {
        return Err(PushNoteError::TooLarge {
            size_bytes: local.len(),
        });
    }
    if local.len() > WARNING_BYTES && !input.confirm_large_file {
        return Err(PushNoteError::LargeFileConfirmationRequired {
            size_bytes: local.len(),
        });
    }
    Ok(local)
}

async fn push_resolved(
    client: &HackmdClient,
    input: PushNoteInput,
    tracked: crate::state::LoadedTrackedState,
    note: crate::note_ref::ResolvedNoteRef,
    remote_note: crate::dto::NoteResponse,
    remote: String,
    local: String,
) -> Result<Result<PushNoteOutput, NoteResolution>, PushNoteError> {
    if matches!(input.strategy, PushStrategy::Safe) {
        if local == tracked.baseline_body {
            let status = if remote == tracked.baseline_body {
                PushStatus::NothingToPush
            } else {
                PushStatus::RemoteChanged
            };
            return Ok(Ok(output(
                status,
                &note.workspace,
                &note.note_id,
                &tracked.state.local_path,
                &tracked.baseline_path,
                false,
            )));
        }
        if remote != tracked.baseline_body {
            return Ok(Ok(conflict_output(
                &note.workspace,
                &note.note_id,
                &tracked.state.local_path,
                &tracked.baseline_path,
                &tracked.baseline_body,
                &local,
                &remote,
            )));
        }
        let recheck = client.get_note(&note.workspace, &note.note_id).await?;
        if recheck.content.as_deref() != Some(tracked.baseline_body.as_str()) {
            return Ok(Ok(conflict_output(
                &note.workspace,
                &note.note_id,
                &tracked.state.local_path,
                &tracked.baseline_path,
                &tracked.baseline_body,
                &local,
                recheck.content.as_deref().unwrap_or_default(),
            )));
        }
    } else if remote == local {
        persist_advanced_state(client, tracked.state, &local, &remote_note)?;
        return Ok(Ok(output(
            PushStatus::NothingToPush,
            &note.workspace,
            &note.note_id,
            &input.local_path,
            &tracked.baseline_path,
            false,
        )));
    }
    client
        .update_note(
            &note.workspace,
            &note.note_id,
            &UpdateNoteRequest {
                content: Some(local.clone()),
                ..UpdateNoteRequest::default()
            },
        )
        .await?;
    let readback = client.get_note(&note.workspace, &note.note_id).await?;
    if readback.content.as_deref() != Some(local.as_str()) {
        return Err(PushNoteError::ReadbackMismatch {
            note_id: note.note_id,
        });
    }
    persist_advanced_state(client, tracked.state, &local, &readback)?;
    Ok(Ok(output(
        PushStatus::Pushed,
        &note.workspace,
        &note.note_id,
        &input.local_path,
        &tracked.baseline_path,
        true,
    )))
}

fn output(
    status: PushStatus,
    workspace: &Workspace,
    note_id: &str,
    local_path: &std::path::Path,
    baseline_path: &std::path::Path,
    pushed: bool,
) -> PushNoteOutput {
    PushNoteOutput {
        status,
        workspace: workspace.clone(),
        note_id: note_id.to_owned(),
        local_path: local_path.to_path_buf(),
        baseline_path: baseline_path.to_path_buf(),
        pushed,
        merge_required: false,
        diff_summary: None,
        snapshot_path: None,
        instructions: None,
    }
}

fn conflict_output(
    workspace: &Workspace,
    note_id: &str,
    local_path: &std::path::Path,
    baseline_path: &std::path::Path,
    baseline: &str,
    local: &str,
    remote: &str,
) -> PushNoteOutput {
    let candidate_snapshot = local_path.with_extension("remote.md");
    PushNoteOutput {
        status: PushStatus::Conflict,
        workspace: workspace.clone(),
        note_id: note_id.to_owned(),
        local_path: local_path.to_path_buf(),
        baseline_path: baseline_path.to_path_buf(),
        pushed: false,
        merge_required: true,
        diff_summary: Some(conflict_diff(baseline, local, remote)),
        snapshot_path: candidate_snapshot.exists().then_some(candidate_snapshot),
        instructions: Some(
            "Merge the local and remote changes; call hackmd_save_remote_snapshot with local_path to save the current remote body. Do not overwrite until the merge is reviewed."
                .to_owned(),
        ),
    }
}

fn conflict_diff(baseline: &str, local: &str, remote: &str) -> String {
    const MAX_CHARS: usize = 4_000;
    let local_diff = TextDiff::from_lines(baseline, local)
        .unified_diff()
        .context_radius(2)
        .header("baseline", "local")
        .to_string();
    let remote_diff = TextDiff::from_lines(baseline, remote)
        .unified_diff()
        .context_radius(2)
        .header("baseline", "remote")
        .to_string();
    let combined = format!("LOCAL CHANGES\n{local_diff}\nREMOTE CHANGES\n{remote_diff}");
    let mut chars = combined.chars();
    let mut bounded = chars.by_ref().take(MAX_CHARS).collect::<String>();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

fn persist_advanced_state(
    client: &HackmdClient,
    mut state: TrackedNoteState,
    body: &str,
    readback: &crate::dto::NoteResponse,
) -> Result<(), PushNoteError> {
    state.baseline_body_hash = format!("sha256:{:x}", Sha256::digest(body.as_bytes()));
    state.last_observed_remote_timestamp = readback
        .last_changed_at
        .map_or_else(String::new, |timestamp| timestamp.to_string());
    state.local_file_identity = local_file_identity(&state.local_path)?;
    client.state().persist_from_sync(&state, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{PushNoteError, PushNoteInput, PushStatus, PushStrategy, conflict_diff, push_note};
    use crate::{
        client::HackmdClient,
        config::Config,
        models::Workspace,
        state::{TrackedNoteState, local_file_identity},
    };

    fn tracked_client(
        fixture: &crate::test_support::SequenceServer,
        root: &Path,
        local_path: &Path,
        baseline: &str,
    ) -> HackmdClient {
        let client = HackmdClient::new(Config::for_loopback_test_with_state(
            &fixture.api_url,
            "fixture-token",
            &root.join("state"),
        ))
        .expect("fixture client should build");
        client
            .state()
            .persist_from_sync(
                &TrackedNoteState {
                    internal_id: "note-id".to_owned(),
                    workspace: Workspace::Personal,
                    local_path: local_path.to_path_buf(),
                    baseline_body_hash: "sha256:fixture".to_owned(),
                    last_observed_remote_timestamp: "1".to_owned(),
                    local_file_identity: local_file_identity(local_path)
                        .expect("local identity should resolve"),
                },
                baseline,
            )
            .expect("tracked state should persist");
        client
    }

    fn input(path: &Path, strategy: PushStrategy, confirm: bool) -> PushNoteInput {
        PushNoteInput {
            workspace: Workspace::Personal,
            note_ref: "note-id".to_owned(),
            local_path: path.to_path_buf(),
            strategy,
            confirm,
            confirm_large_file: false,
        }
    }

    #[tokio::test]
    async fn safe_push_rechecks_patches_reads_back_and_advances_baseline() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");
        let fixture = crate::test_support::SequenceServer::spawn([
            (
                200,
                r#"{"id":"note-id","title":"Note","content":"baseline"}"#,
            ),
            (
                200,
                r#"{"id":"note-id","title":"Note","content":"baseline"}"#,
            ),
            (202, ""),
            (
                200,
                r#"{"id":"note-id","title":"Note","content":"local edit","lastChangedAt":2}"#,
            ),
        ]);
        let client = tracked_client(&fixture, directory.path(), &local_path, "baseline");
        let output = push_note(&client, input(&local_path, PushStrategy::Safe, false))
            .await
            .expect("safe push should succeed")
            .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::Pushed);
        assert!(output.pushed);
        let requests = fixture.finish();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].starts_with("PATCH /v1/notes/note-id HTTP/1.1\r\n"));
        assert!(requests[2].ends_with(r#"{"content":"local edit"}"#));
        let loaded = client
            .state()
            .load_for_local_path(&local_path)
            .expect("advanced state should load");
        assert_eq!(loaded.baseline_body, "local edit");
        assert_eq!(loaded.state.last_observed_remote_timestamp, "2");
    }

    #[tokio::test]
    async fn safe_push_reports_conflict_without_patch() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");
        fs::write(local_path.with_extension("remote.md"), "prior snapshot")
            .expect("snapshot fixture should write");
        let fixture = crate::test_support::SequenceServer::spawn([(
            200,
            r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
        )]);
        let client = tracked_client(&fixture, directory.path(), &local_path, "baseline");
        let output = push_note(&client, input(&local_path, PushStrategy::Safe, false))
            .await
            .expect("comparison should succeed")
            .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::Conflict);
        assert!(!output.pushed);
        assert!(
            output
                .baseline_path
                .ends_with("personal--note-id.baseline.md")
        );
        assert_eq!(output.local_path, local_path);
        assert!(output.merge_required);
        assert!(output.snapshot_path.is_some());
        assert!(
            output
                .instructions
                .as_deref()
                .expect("instructions should exist")
                .contains("hackmd_save_remote_snapshot")
        );
        let diff = output.diff_summary.expect("diff summary should exist");
        assert!(diff.contains("LOCAL CHANGES"));
        assert!(diff.contains("REMOTE CHANGES"));
        assert_eq!(fixture.finish().len(), 1);
    }

    #[test]
    fn conflict_diff_is_bounded() {
        let baseline = "base\n".repeat(2_000);
        let local = "local\n".repeat(2_000);
        let remote = "remote\n".repeat(2_000);
        let summary = conflict_diff(&baseline, &local, &remote);
        assert!(summary.chars().count() <= 4_001);
        assert!(summary.ends_with('…'));
    }

    #[tokio::test]
    async fn unchanged_local_reports_remote_change_without_patch() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let fixture = crate::test_support::SequenceServer::spawn([(
            200,
            r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
        )]);
        let client = tracked_client(&fixture, directory.path(), &local_path, "baseline");
        let output = push_note(&client, input(&local_path, PushStrategy::Safe, false))
            .await
            .expect("comparison should succeed")
            .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::RemoteChanged);
        assert_eq!(fixture.finish().len(), 1);
    }

    #[tokio::test]
    async fn overwrite_requires_confirmation_before_network() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local").expect("local fixture should write");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        assert!(matches!(
            push_note(&client, input(&local_path, PushStrategy::Overwrite, false)).await,
            Err(PushNoteError::OverwriteConfirmationRequired)
        ));
    }

    #[tokio::test]
    async fn confirmed_overwrite_patches_and_reads_back() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "forced local").expect("local fixture should write");
        let fixture = crate::test_support::SequenceServer::spawn([
            (
                200,
                r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
            ),
            (202, ""),
            (
                200,
                r#"{"id":"note-id","title":"Note","content":"forced local","lastChangedAt":3}"#,
            ),
        ]);
        let client = tracked_client(&fixture, directory.path(), &local_path, "baseline");
        let output = push_note(&client, input(&local_path, PushStrategy::Overwrite, true))
            .await
            .expect("overwrite should succeed")
            .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::Pushed);
        let requests = fixture.finish();
        assert!(requests[1].starts_with("PATCH /v1/notes/note-id HTTP/1.1\r\n"));
        assert!(requests[2].starts_with("GET /v1/notes/note-id HTTP/1.1\r\n"));
    }
}
