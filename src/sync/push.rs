use std::{
    fs,
    path::{Path, PathBuf},
};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::UpdateNoteRequest,
    local::{LocalAccessError, LocalFiles},
    models::Workspace,
    note::reference::{NoteRefError, NoteResolution},
    sync::state::{StateError, TrackedNoteState},
    sync::{BODY_MAX_BYTES, BODY_WARNING_BYTES},
};

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
    /// Bypass cached note lists when resolving an `@owner/slug` URL.
    #[serde(default)]
    pub(crate) refresh: bool,
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
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error("local_path must be absolute")]
    RelativePath,
    #[error("local_path must be a readable regular file")]
    InvalidLocalFile,
    #[error("overwrite strategy requires confirm: true")]
    OverwriteConfirmationRequired,
    #[error("local file is {size_bytes} bytes; retry with confirm_large_file: true")]
    LargeFileConfirmationRequired { size_bytes: usize },
    #[error(
        "local file is {size_bytes} bytes; files above {} MiB are refused",
        BODY_MAX_BYTES / 1024 / 1024
    )]
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
    files: &LocalFiles,
    input: PushNoteInput,
) -> Result<Result<PushNoteOutput, NoteResolution>, PushNoteError> {
    if !input.local_path.is_absolute() {
        return Err(PushNoteError::RelativePath);
    }
    files.allow(&input.local_path)?;
    let local = validate_and_read_local(&input)?;
    let tracked = files.state().load_for_local_path(&input.local_path)?;
    let resolution = crate::note::reference::resolve_note_ref(
        client,
        input.workspace.clone(),
        &input.note_ref,
        input.refresh,
    )
    .await?;
    let NoteResolution::Resolved { note } = resolution else {
        return Ok(Err(resolution));
    };
    if note.note_id != tracked.state.internal_id || note.workspace != tracked.state.workspace {
        return Err(PushNoteError::TrackingMismatch);
    }
    let remote_note = client.get_note(&note.workspace, &note.note_id).await?;
    push_resolved(
        client,
        files,
        input.strategy,
        tracked,
        note,
        remote_note,
        local,
    )
    .await
}

fn validate_and_read_local(input: &PushNoteInput) -> Result<String, PushNoteError> {
    if matches!(input.strategy, PushStrategy::Overwrite) && !input.confirm {
        return Err(PushNoteError::OverwriteConfirmationRequired);
    }
    let metadata = fs::metadata(&input.local_path).map_err(|_| PushNoteError::InvalidLocalFile)?;
    if !metadata.is_file() {
        return Err(PushNoteError::InvalidLocalFile);
    }
    let local = fs::read_to_string(&input.local_path).map_err(PushNoteError::LocalIo)?;
    validate_body_size(local.len(), input.confirm_large_file)?;
    Ok(local)
}

fn validate_body_size(size_bytes: usize, confirmed: bool) -> Result<(), PushNoteError> {
    if size_bytes > BODY_MAX_BYTES {
        return Err(PushNoteError::TooLarge { size_bytes });
    }
    if size_bytes > BODY_WARNING_BYTES && !confirmed {
        return Err(PushNoteError::LargeFileConfirmationRequired { size_bytes });
    }
    Ok(())
}

async fn push_resolved(
    client: &HackmdClient,
    files: &LocalFiles,
    strategy: PushStrategy,
    tracked: crate::sync::state::LoadedTrackedState,
    note: crate::note::reference::ResolvedNoteRef,
    remote_note: crate::dto::NoteResponse,
    local: String,
) -> Result<Result<PushNoteOutput, NoteResolution>, PushNoteError> {
    let remote_timestamp = remote_note.last_changed_at;
    let remote = remote_note
        .content
        .ok_or_else(|| PushNoteError::MissingRemoteContent {
            note_id: note.note_id.clone(),
        })?;

    // Every result reports the tracked canonical path, not the caller's
    // spelling of it, so the snapshot path here is the one
    // hackmd_save_remote_snapshot would write.
    let target = Target {
        workspace: &note.workspace,
        note_id: &note.note_id,
        local_path: &tracked.state.local_path,
        baseline_path: &tracked.baseline_path,
    };
    if matches!(strategy, PushStrategy::Safe) {
        if local == tracked.baseline_body {
            let status = if remote == tracked.baseline_body {
                PushStatus::NothingToPush
            } else {
                PushStatus::RemoteChanged
            };
            return Ok(Ok(output(&target, status, false)));
        }

        // The remote body was read once, in push_note, and only local string
        // comparisons have happened since. A second read here would observe the
        // same instant at the cost of another round trip, and it could not
        // close the window that matters anyway: HackMD has no conditional
        // write, so an edit landing between this check and the PATCH below is
        // overwritten either way.
        if remote != tracked.baseline_body {
            return Ok(Ok(conflict_output(
                &target,
                &tracked.baseline_body,
                &local,
                &remote,
            )));
        }
    } else if remote == local {
        let result = output(&target, PushStatus::NothingToPush, false);
        advance_state(files, tracked.state, &local, remote_timestamp)?;
        return Ok(Ok(result));
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
    let readback = crate::client::poll_readback(
        || client.get_note(&note.workspace, &note.note_id),
        |readback| readback.content.as_deref() == Some(local.as_str()),
    )
    .await?;
    if !readback.confirmed {
        return Err(PushNoteError::ReadbackMismatch {
            note_id: note.note_id,
        });
    }
    let result = output(&target, PushStatus::Pushed, true);
    advance_state(files, tracked.state, &local, readback.value.last_changed_at)?;
    Ok(Ok(result))
}

/// The note and the two files every push result names, borrowed once so the
/// result builders do not repeat them.
struct Target<'a> {
    workspace: &'a Workspace,
    note_id: &'a str,
    local_path: &'a Path,
    baseline_path: &'a Path,
}

fn output(target: &Target<'_>, status: PushStatus, pushed: bool) -> PushNoteOutput {
    PushNoteOutput {
        status,
        workspace: target.workspace.clone(),
        note_id: target.note_id.to_owned(),
        local_path: target.local_path.to_path_buf(),
        baseline_path: target.baseline_path.to_path_buf(),
        pushed,
        merge_required: false,
        diff_summary: None,
        snapshot_path: None,
        instructions: None,
    }
}

fn conflict_output(
    target: &Target<'_>,
    baseline: &str,
    local: &str,
    remote: &str,
) -> PushNoteOutput {
    let candidate_snapshot = target.local_path.with_extension("remote.md");
    PushNoteOutput {
        merge_required: true,
        diff_summary: Some(conflict_diff(baseline, local, remote)),
        snapshot_path: candidate_snapshot.exists().then_some(candidate_snapshot),
        instructions: Some(
            "Merge the local and remote changes; call hackmd_save_remote_snapshot with local_path to save the current remote body. Do not overwrite until the merge is reviewed."
                .to_owned(),
        ),
        ..output(target, PushStatus::Conflict, false)
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

fn advance_state(
    files: &LocalFiles,
    state: TrackedNoteState,
    body: &str,
    remote_timestamp: Option<i64>,
) -> Result<(), PushNoteError> {
    let state = state.advance(body, remote_timestamp)?;
    files.state().persist_from_sync(&state, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        PushNoteError, PushNoteInput, PushStatus, PushStrategy, conflict_diff, push_note,
        validate_body_size,
    };
    use crate::{
        client::HackmdClient,
        config::Config,
        models::Workspace,
        sync::{BODY_MAX_BYTES, BODY_WARNING_BYTES},
    };

    fn input(path: &Path, strategy: PushStrategy, confirm: bool) -> PushNoteInput {
        PushNoteInput {
            workspace: Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            local_path: path.to_path_buf(),
            strategy,
            confirm,
            confirm_large_file: false,
        }
    }

    #[tokio::test]
    async fn a_push_that_never_becomes_visible_is_an_error() {
        const BASELINE: &str = r#"{"id":"note-id","title":"Note","content":"baseline"}"#;
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");
        // Read, PATCH, then a read-back that keeps showing the old body.
        let fixture = crate::fixture::SequenceServer::spawn_repeating([
            (200, BASELINE),
            (202, ""),
            (200, BASELINE),
        ]);
        let client = fixture.client();
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");

        assert!(matches!(
            push_note(
                &client,
                &files,
                input(&local_path, PushStrategy::Safe, false)
            )
            .await,
            Err(PushNoteError::ReadbackMismatch { .. })
        ));
        // The baseline must not advance to a body HackMD never confirmed.
        let loaded = files
            .state()
            .load_for_local_path(&local_path)
            .expect("state should still load");
        assert_eq!(loaded.baseline_body, "baseline");
    }

    #[tokio::test]
    async fn safe_push_patches_reads_back_and_advances_baseline() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");

        // One read, one PATCH, one read-back: the safe strategy compares
        // against the body it already fetched rather than fetching it twice.
        let fixture = crate::fixture::SequenceServer::spawn([
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
        let client = fixture.client();
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let output = push_note(
            &client,
            &files,
            input(&local_path, PushStrategy::Safe, false),
        )
        .await
        .expect("safe push should succeed")
        .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::Pushed);
        assert!(output.pushed);
        let requests = fixture.finish();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("PATCH /v1/notes/note-id HTTP/1.1\r\n"));
        assert!(requests[1].ends_with(r#"{"content":"local edit"}"#));
        let loaded = files
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
        let fixture = crate::fixture::SequenceServer::spawn([(
            200,
            r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
        )]);
        let client = fixture.client();
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let output = push_note(
            &client,
            &files,
            input(&local_path, PushStrategy::Safe, false),
        )
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
        let fixture = crate::fixture::SequenceServer::spawn([(
            200,
            r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
        )]);
        let client = fixture.client();
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let output = push_note(
            &client,
            &files,
            input(&local_path, PushStrategy::Safe, false),
        )
        .await
        .expect("comparison should succeed")
        .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::RemoteChanged);
        assert_eq!(fixture.finish().len(), 1);
    }

    #[tokio::test]
    async fn unchanged_local_and_remote_is_a_no_op() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let fixture = crate::fixture::SequenceServer::spawn([(
            200,
            r#"{"id":"note-id","title":"Note","content":"baseline"}"#,
        )]);
        let client = fixture.client();
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let output = push_note(
            &client,
            &files,
            input(&local_path, PushStrategy::Safe, false),
        )
        .await
        .expect("comparison should succeed")
        .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::NothingToPush);
        assert!(!output.pushed);
        assert_eq!(fixture.finish().len(), 1);
    }

    #[tokio::test]
    async fn overwrite_requires_confirmation_before_network() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local").expect("local fixture should write");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let files = crate::fixture::unconfined_files(directory.path().join("state"));
        assert!(matches!(
            push_note(
                &client,
                &files,
                input(&local_path, PushStrategy::Overwrite, false)
            )
            .await,
            Err(PushNoteError::OverwriteConfirmationRequired)
        ));
    }

    #[tokio::test]
    async fn confirmed_overwrite_patches_and_reads_back() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "forced local").expect("local fixture should write");
        let fixture = crate::fixture::SequenceServer::spawn([
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
        let client = fixture.client();
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let output = push_note(
            &client,
            &files,
            input(&local_path, PushStrategy::Overwrite, true),
        )
        .await
        .expect("overwrite should succeed")
        .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::Pushed);
        let requests = fixture.finish();
        assert!(requests[1].starts_with("PATCH /v1/notes/note-id HTTP/1.1\r\n"));
        assert!(requests[2].starts_with("GET /v1/notes/note-id HTTP/1.1\r\n"));
    }

    #[test]
    fn push_body_limits_have_exact_boundaries() {
        assert!(validate_body_size(BODY_WARNING_BYTES, false).is_ok());
        assert!(matches!(
            validate_body_size(BODY_WARNING_BYTES + 1, false),
            Err(PushNoteError::LargeFileConfirmationRequired { .. })
        ));
        assert!(validate_body_size(BODY_WARNING_BYTES + 1, true).is_ok());
        assert!(validate_body_size(BODY_MAX_BYTES, true).is_ok());
        assert!(matches!(
            validate_body_size(BODY_MAX_BYTES + 1, true),
            Err(PushNoteError::TooLarge { .. })
        ));
    }
}
