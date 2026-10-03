use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    local::{LocalAccessError, LocalFiles},
    models::Workspace,
    note::reference::{NoteRefError, NoteResolution},
    sync::state::{StateError, TrackedNoteState, body_digest, body_hash, body_hash_from_digest},
    sync::{BODY_MAX_BYTES, ChangeState, LocalBodyError, classify_changes, read_local_body},
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
    /// Absolute path of a Markdown file tracked by an earlier
    /// `hackmd_pull_note`. Its sync record names the note to push to.
    pub(crate) local_path: PathBuf,
    /// Optional cross-check: when given, the push is refused unless this
    /// reference resolves to the note the sync record names. Omit it to save
    /// the lookup.
    pub(crate) note_ref: Option<String>,
    /// Team path (from `hackmd_get_me`) for a direct internal `note_ref`.
    /// Omitted, the tracked note's own workspace is assumed; `@owner/slug`
    /// URLs name their own.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Bypass the 60-second account and note-list caches when resolving an
    /// `@owner/slug` `note_ref`.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// `safe` (default) writes only when the remote still matches the
    /// baseline; `overwrite` replaces the remote body unconditionally.
    #[serde(default = "default_strategy")]
    #[schemars(default = "default_strategy")]
    pub(crate) strategy: PushStrategy,
    /// Required with `strategy: overwrite`.
    #[serde(default)]
    pub(crate) confirm: bool,
    /// After merging a conflict: the `remote_body_hash` that conflict
    /// reported. The safe push then writes the merged file only if the remote
    /// is still exactly the body that was merged, and conflicts again if it
    /// changed since.
    pub(crate) expected_remote_hash: Option<String>,
    /// Required to push a file above 5 MiB.
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
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    pub(crate) local_path: PathBuf,
    pub(crate) baseline_path: PathBuf,
    /// On a conflict, the hash to pass back as `expected_remote_hash` once
    /// the snapshot has been merged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remote_body_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) diff_summary: Option<String>,
    /// On a conflict, the sibling `*.remote.md` just written with the current
    /// remote body, so both sides can be merged without another request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snapshot_path: Option<PathBuf>,
    /// Why no snapshot was written on a conflict, when one could not be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snapshot_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum PushNoteError {
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error(transparent)]
    LocalBody(#[from] LocalBodyError),
    #[error("overwrite strategy requires confirm: true")]
    OverwriteConfirmationRequired,
    #[error("note_ref resolves to a different note than the one local_path was pulled from")]
    TrackingMismatch,
    #[error(
        "expected_remote_hash must be the remote_body_hash a conflict reported: sha256: and 64 lowercase hex digits"
    )]
    MalformedExpectedHash,
    #[error(
        "HackMD updated note {note_id}, but local sync state could not be persisted; run hackmd_pull_note with overwrite_local: true before the next push"
    )]
    StatePersistenceAfterWrite {
        note_id: String,
        #[source]
        source: Box<StateError>,
    },
    #[error(
        "{} exists and is not the snapshot this tool last wrote, so it was left alone; move it to get a fresh snapshot",
        path.display()
    )]
    SnapshotNotOurs { path: PathBuf },
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

impl crate::reply::ToolError for PushNoteError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::Access(error) => error.kind(),
            Self::LocalBody(error) => error.kind(),
            Self::TrackingMismatch | Self::MalformedExpectedHash => ErrorKind::InvalidInput,
            Self::OverwriteConfirmationRequired => ErrorKind::ConfirmationRequired,
            Self::StatePersistenceAfterWrite { .. } => ErrorKind::PartialWrite,
            Self::SnapshotNotOurs { .. } => ErrorKind::LocalAccess,
            Self::Reference(error) => error.kind(),
            Self::State(error) => error.kind(),
            Self::Api(error) => error.kind(),
        }
    }
}

pub(crate) async fn push_note(
    client: &HackmdClient,
    files: &LocalFiles,
    input: PushNoteInput,
) -> Result<Result<PushNoteOutput, NoteResolution>, PushNoteError> {
    files.allow(&input.local_path)?;
    let local = validate_and_read_local(files, &input)?;
    let tracked = files.state().load_for_local_path(&input.local_path)?;
    let note = crate::note::reference::ResolvedNoteRef {
        workspace: tracked.state.workspace.clone(),
        note_id: tracked.state.internal_id.clone(),
    };
    if let Some(note_ref) = input.note_ref.as_deref() {
        // A bare note ID without team_path is read in the tracked note's own
        // workspace: the cross-check is about which note, and omitting
        // team_path should not make a team note look like a different one.
        let workspace = match input.workspace {
            Workspace::Personal => note.workspace.clone(),
            team @ Workspace::Team { .. } => team,
        };
        let resolution =
            crate::note::reference::resolve_note_ref(client, workspace, note_ref, input.refresh)
                .await?;
        let NoteResolution::Resolved { note: named } = resolution else {
            return Ok(Err(resolution));
        };
        if named != note {
            return Err(PushNoteError::TrackingMismatch);
        }
    }
    let (remote_note, remote) = client.get_note_body(&note.workspace, &note.note_id).await?;
    push_resolved(
        client,
        files,
        Guard {
            strategy: input.strategy,
            expected_remote_hash: input.expected_remote_hash.as_deref(),
        },
        tracked,
        note,
        Remote {
            body: remote,
            last_changed_at: remote_note.last_changed_at,
        },
        local,
    )
    .await
}

/// The remote body a push is judged against, and when it last changed.
struct Remote {
    body: String,
    last_changed_at: Option<i64>,
}

fn validate_and_read_local(
    files: &LocalFiles,
    input: &PushNoteInput,
) -> Result<String, PushNoteError> {
    if input
        .expected_remote_hash
        .as_deref()
        .is_some_and(|hash| !crate::sync::state::is_body_hash(hash))
    {
        return Err(PushNoteError::MalformedExpectedHash);
    }
    if matches!(input.strategy, PushStrategy::Overwrite) && !input.confirm {
        return Err(PushNoteError::OverwriteConfirmationRequired);
    }
    Ok(read_local_body(
        files,
        &input.local_path,
        input.confirm_large_file,
    )?)
}

async fn push_resolved(
    client: &HackmdClient,
    files: &LocalFiles,
    guard: Guard<'_>,
    tracked: crate::sync::state::LoadedTrackedState,
    note: crate::note::reference::ResolvedNoteRef,
    remote: Remote,
    local: String,
) -> Result<Result<PushNoteOutput, NoteResolution>, PushNoteError> {
    let Remote {
        body: remote,
        last_changed_at: remote_timestamp,
    } = remote;

    // Every result reports the tracked canonical path, not the caller's
    // spelling of it, and the conflict snapshot is written beside that path.
    let target = Target {
        workspace: &note.workspace,
        note_id: &note.note_id,
        local_path: &tracked.state.local_path,
        baseline_path: &tracked.baseline_path,
    };
    if matches!(guard.strategy, PushStrategy::Safe) {
        let (local_digest, remote_digest) =
            crate::local::offload(|| (body_digest(&local), body_digest(&remote)));
        let remote_hash = body_hash_from_digest(&remote_digest);

        // A merge names the remote it was built against. If the remote has
        // moved since, even back to the baseline, writing the merge would bring
        // back whatever that move removed.
        let merged_against_other_remote = guard
            .expected_remote_hash
            .is_some_and(|expected| expected != remote_hash);

        // Always judged against the recorded baseline first, so a remote-only
        // change is never mistaken for a local one, whatever hash is supplied.
        match classify_changes(&tracked.baseline_digest, &local_digest, &remote_digest) {
            ChangeState::InSync => {
                return Ok(Ok(output(&target, PushStatus::NothingToPush)));
            }

            // Both sides changed to the same body, as after identical edits or
            // a merge that settled on the remote: nothing to write, but the
            // baseline moves there, or the next check reports a conflict.
            ChangeState::Conflict if local_digest == remote_digest => {
                let result = output(&target, PushStatus::NothingToPush);
                advance_state(files, tracked.state, &local, remote_timestamp)?;
                return Ok(Ok(result));
            }

            // A merge built against exactly this remote body may replace it.
            // Anything newer on the remote is still a conflict below.
            ChangeState::Conflict if guard.expected_remote_hash == Some(remote_hash.as_str()) => {}
            ChangeState::RemoteOnly => {
                return Ok(Ok(PushNoteOutput {
                    instructions: Some(REMOTE_CHANGED_INSTRUCTIONS.to_owned()),
                    ..output(&target, PushStatus::RemoteChanged)
                }));
            }
            ChangeState::LocalOnly if !merged_against_other_remote => {}
            ChangeState::Conflict | ChangeState::LocalOnly => {
                // The conflict is the result; a snapshot that cannot be saved
                // is reported beside it rather than replacing it with an error.
                let (snapshot_path, snapshot_error) =
                    match write_snapshot(files, tracked.state.clone(), &remote, &remote_hash) {
                        Ok(path) => (Some(path), None),
                        Err(error) => (None, Some(error.to_string())),
                    };
                return Ok(Ok(PushNoteOutput {
                    remote_body_hash: Some(remote_hash),
                    diff_summary: Some(conflict_diff(&tracked.baseline_body, &local, &remote)),
                    snapshot_path,
                    snapshot_error,
                    instructions: Some(CONFLICT_INSTRUCTIONS.to_owned()),
                    ..output(&target, PushStatus::Conflict)
                }));
            }
        }
    } else if remote == local {
        let result = output(&target, PushStatus::NothingToPush);
        advance_state(files, tracked.state, &local, remote_timestamp)?;
        return Ok(Ok(result));
    }
    let written = client
        .write_note_body(&note.workspace, &note.note_id, &local)
        .await?;
    let result = output(&target, PushStatus::Pushed);
    advance_state(files, tracked.state, &local, written.last_changed_at).map_err(|source| {
        PushNoteError::StatePersistenceAfterWrite {
            note_id: note.note_id,
            source: Box::new(source),
        }
    })?;
    Ok(Ok(result))
}

/// What a push may overwrite: the strategy, and for a safe push the remote
/// body a merge was built from.
#[derive(Clone, Copy)]
struct Guard<'a> {
    strategy: PushStrategy,
    expected_remote_hash: Option<&'a str>,
}

/// The note and the two files every push result names, borrowed once so the
/// result builders do not repeat them.
struct Target<'a> {
    workspace: &'a Workspace,
    note_id: &'a str,
    local_path: &'a Path,
    baseline_path: &'a Path,
}

fn output(target: &Target<'_>, status: PushStatus) -> PushNoteOutput {
    PushNoteOutput {
        status,
        workspace: target.workspace.clone(),
        note_id: target.note_id.to_owned(),
        local_path: target.local_path.to_path_buf(),
        baseline_path: target.baseline_path.to_path_buf(),
        remote_body_hash: None,
        diff_summary: None,
        snapshot_path: None,
        snapshot_error: None,
        instructions: None,
    }
}

const REMOTE_CHANGED_INSTRUCTIONS: &str = "Only the remote changed since the last sync; hackmd_pull_note with overwrite_local: true brings the local file up to date.";

const CONFLICT_INSTRUCTIONS: &str = "Both sides changed since the last sync. Merge snapshot_path (the current remote body) into local_path, then push again with expected_remote_hash set to remote_body_hash; that push writes only if the remote has not changed again. Do not use strategy: overwrite until the merge is reviewed.";

/// Saves the remote body as `<name>.remote.md` beside the working file, and
/// records its hash so the next conflict knows the file is its own. An
/// existing file is replaced only while it still has the recorded content: a
/// file the user wrote, or edited while merging, is never overwritten.
fn write_snapshot(
    files: &LocalFiles,
    mut state: TrackedNoteState,
    remote: &str,
    remote_hash: &str,
) -> Result<PathBuf, PushNoteError> {
    let snapshot_path = state.local_path.with_extension("remote.md");
    files.allow_write(&snapshot_path)?;
    if files.entry(&snapshot_path)?.is_some() {
        let existing = files.read_capped(&snapshot_path, BODY_MAX_BYTES + 1)?;
        let ours = crate::local::offload(|| std::str::from_utf8(&existing).ok().map(body_hash));
        if ours.is_none() || ours != state.remote_snapshot_hash {
            return Err(PushNoteError::SnapshotNotOurs {
                path: snapshot_path,
            });
        }
    }
    files.write_atomic(&snapshot_path, remote.as_bytes(), false)?;
    state.remote_snapshot_hash = Some(remote_hash.to_owned());

    // A snapshot whose hash never reached the record would read as someone
    // else's file from the next conflict on, so it does not stay behind.
    if let Err(error) = files.state().update_sidecar(&state) {
        let _ = files.remove_file(&snapshot_path);
        return Err(error.into());
    }
    Ok(snapshot_path)
}

fn conflict_diff(baseline: &str, local: &str, remote: &str) -> String {
    const DIFF_BYTES: usize = 1_900;
    let baseline = bounded_diff_input(baseline);
    let local = bounded_diff_input(local);
    let remote = bounded_diff_input(remote);
    let local_diff = bounded_unified_diff(&baseline, &local, "local", DIFF_BYTES);
    let remote_diff = bounded_unified_diff(&baseline, &remote, "remote", DIFF_BYTES);
    format!("LOCAL CHANGES\n{local_diff}\nREMOTE CHANGES\n{remote_diff}")
}

fn bounded_diff_input(value: &str) -> Cow<'_, str> {
    // Each side can emit at most 1.9 KiB. Keeping 8 KiB from both ends gives
    // the formatter ample context while preventing `similar` from indexing
    // hundreds of KiB that can never reach the result.
    const MAX_INPUT_BYTES: usize = 16 * 1024;
    if value.len() <= MAX_INPUT_BYTES {
        return Cow::Borrowed(value);
    }

    let mut head_end = MAX_INPUT_BYTES / 2;
    while !value.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = value.len() - MAX_INPUT_BYTES / 2;
    while !value.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start - head_end;
    Cow::Owned(format!(
        "[… {omitted} bytes omitted from diff input …]\n{}\n{}",
        &value[..head_end],
        &value[tail_start..]
    ))
}

fn bounded_unified_diff(baseline: &str, changed: &str, label: &str, limit: usize) -> String {
    let diff = TextDiff::from_lines(baseline, changed);
    let mut writer = BoundedWriter::new(limit);
    let _ = diff
        .unified_diff()
        .context_radius(2)
        .header("baseline", label)
        .to_writer(&mut writer);
    writer.finish()
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit + "…".len()),
            limit,
            truncated: false,
        }
    }

    fn finish(mut self) -> String {
        if self.truncated {
            self.bytes.extend_from_slice("…".as_bytes());
        }
        String::from_utf8(self.bytes).expect("diff formatter writes valid UTF-8")
    }
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if buffer.len() <= remaining {
            self.bytes.extend_from_slice(buffer);
            return Ok(buffer.len());
        }

        let text = std::str::from_utf8(buffer).map_err(std::io::Error::other)?;
        let mut end = remaining.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.bytes.extend_from_slice(&buffer[..end]);
        self.truncated = true;
        Ok(end)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn advance_state(
    files: &LocalFiles,
    state: TrackedNoteState,
    body: &str,
    remote_timestamp: Option<i64>,
) -> Result<(), StateError> {
    let state = state.advance(body, remote_timestamp)?;
    files.state().persist_from_sync(&state, body)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        PushNoteError, PushNoteInput, PushStatus, PushStrategy, conflict_diff, push_note,
        push_resolved,
    };
    use crate::{
        client::{HackmdClient, HackmdError},
        config::Config,
        fixture::{Scenario, SequenceServer},
        models::Workspace,
        sync::BODY_MAX_BYTES,
    };

    fn input(path: &Path, strategy: PushStrategy, confirm: bool) -> PushNoteInput {
        PushNoteInput {
            local_path: path.to_path_buf(),
            note_ref: None,
            workspace: Workspace::Personal,
            refresh: false,
            strategy,
            confirm,
            expected_remote_hash: None,
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
            Err(PushNoteError::Api(HackmdError::ReadbackMismatch { .. }))
        ));
        // The baseline must not advance to a body HackMD never confirmed.
        let loaded = files
            .state()
            .load_for_local_path(&local_path)
            .expect("state should still load");
        assert_eq!(loaded.baseline_body, "baseline");
    }

    #[tokio::test]
    async fn confirmed_remote_write_with_failed_state_persistence_is_recoverable() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let tracked = files
            .state()
            .load_for_local_path(&local_path)
            .expect("tracked state should load before sabotage");

        let state_root = directory.path().join("state");
        fs::remove_dir_all(&state_root).expect("state fixture should be removable");
        fs::write(&state_root, "blocks directory recreation")
            .expect("blocking state file should write");

        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("PATCH", "/v1/notes/note-id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"local edit","lastChangedAt":2}"#,
            ),
        ]);
        let result = push_resolved(
            &fixture.client(),
            &files,
            super::Guard {
                strategy: PushStrategy::Safe,
                expected_remote_hash: None,
            },
            tracked,
            crate::note::reference::ResolvedNoteRef {
                workspace: Workspace::Personal,
                note_id: "note-id".to_owned(),
            },
            super::Remote {
                body: "baseline".to_owned(),
                last_changed_at: None,
            },
            "local edit".to_owned(),
        )
        .await;

        assert!(matches!(
            result,
            Err(PushNoteError::StatePersistenceAfterWrite { note_id, .. })
                if note_id == "note-id"
        ));
        fixture.finish();
    }

    #[tokio::test]
    async fn failed_remote_write_does_not_advance_local_state() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let tracked = files
            .state()
            .load_for_local_path(&local_path)
            .expect("tracked state should load");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "PATCH",
            "/v1/notes/note-id",
            400,
            r#"{"error":"rejected"}"#,
        )]);

        let result = push_resolved(
            &fixture.client_without_retry("fixture-token"),
            &files,
            super::Guard {
                strategy: PushStrategy::Safe,
                expected_remote_hash: None,
            },
            tracked,
            crate::note::reference::ResolvedNoteRef {
                workspace: Workspace::Personal,
                note_id: "note-id".to_owned(),
            },
            super::Remote {
                body: "baseline".to_owned(),
                last_changed_at: None,
            },
            "local edit".to_owned(),
        )
        .await;

        assert!(matches!(result, Err(PushNoteError::Api(_))));
        assert_eq!(
            files
                .state()
                .load_for_local_path(&local_path)
                .expect("unchanged state should load")
                .baseline_body,
            "baseline"
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn safe_push_patches_reads_back_and_advances_baseline() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");

        // One read, one PATCH, one read-back: the safe strategy compares
        // against the body it already fetched rather than fetching it twice.
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"baseline"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, "")
                .expect_body("the complete local body", |body| {
                    body == r#"{"content":"local edit"}"#
                }),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
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
        fixture.finish();
        let loaded = files
            .state()
            .load_for_local_path(&local_path)
            .expect("advanced state should load");
        assert_eq!(loaded.baseline_body, "local edit");
        assert_eq!(loaded.state.last_observed_remote_timestamp, "2");
    }

    #[tokio::test]
    async fn safe_push_reports_conflict_without_patch() {
        const REMOTE: &str = r#"{"id":"note-id","title":"Note","content":"remote edit"}"#;
        const NEWER: &str = r#"{"id":"note-id","title":"Note","content":"newer remote"}"#;
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        let snapshot = local_path.with_extension("remote.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");
        fs::write(&snapshot, "user notes").expect("unrelated file should write");
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/notes/note-id", 200, REMOTE),
            Scenario::new("GET", "/v1/notes/note-id", 200, REMOTE),
            Scenario::new("GET", "/v1/notes/note-id", 200, NEWER),
        ]);
        let client = fixture.client();
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let push = || {
            push_note(
                &client,
                &files,
                input(&local_path, PushStrategy::Safe, false),
            )
        };

        // A file this tool did not write is left alone, and the conflict is
        // still reported in full.
        let output = push()
            .await
            .expect("comparison should succeed")
            .expect("direct note should resolve");
        assert_eq!(output.status, PushStatus::Conflict);
        assert!(
            output
                .baseline_path
                .ends_with("personal--note%2Did.baseline.md")
        );
        assert_eq!(output.local_path, local_path);
        assert!(output.snapshot_path.is_none());
        assert!(output.snapshot_error.is_some());
        assert_eq!(
            fs::read_to_string(&snapshot).expect("user file should read"),
            "user notes"
        );
        let diff = output.diff_summary.expect("diff summary should exist");
        assert!(diff.contains("LOCAL CHANGES"));
        assert!(diff.contains("REMOTE CHANGES"));

        // Once it is gone, the conflict writes its own snapshot, and a later
        // conflict may replace that one.
        fs::remove_file(&snapshot).expect("user file should remove");
        for expected in ["remote edit", "newer remote"] {
            let output = push()
                .await
                .expect("comparison should succeed")
                .expect("direct note should resolve");
            assert_eq!(
                output.snapshot_path.as_deref(),
                Some(snapshot.as_path()),
                "{:?}",
                output.snapshot_error
            );
            assert_eq!(
                fs::read_to_string(&snapshot).expect("snapshot should read"),
                expected
            );
        }
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should read"),
            "local edit"
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn a_merged_push_writes_only_against_the_remote_it_merged() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, "")
                .expect_body("the merged body", |body| body == r#"{"content":"merged"}"#),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"merged","lastChangedAt":4}"#,
            ),
        ]);
        let client = fixture.client();
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");

        let conflict = push_note(
            &client,
            &files,
            input(&local_path, PushStrategy::Safe, false),
        )
        .await
        .expect("comparison should succeed")
        .expect("tracked note should resolve");
        assert_eq!(conflict.status, PushStatus::Conflict);
        let remote_hash = conflict.remote_body_hash.expect("conflict reports a hash");

        fs::write(&local_path, "merged").expect("merge should write");
        let mut merged = input(&local_path, PushStrategy::Safe, false);
        merged.expected_remote_hash = Some(remote_hash);
        let pushed = push_note(&client, &files, merged)
            .await
            .expect("merged push should succeed")
            .expect("tracked note should resolve");
        assert_eq!(pushed.status, PushStatus::Pushed);
        assert_eq!(
            files
                .state()
                .load_for_local_path(&local_path)
                .expect("advanced state should load")
                .baseline_body,
            "merged"
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn a_supplied_hash_never_turns_a_remote_change_into_a_push() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
        )]);
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let mut stale = input(&local_path, PushStrategy::Safe, false);
        stale.expected_remote_hash = Some(crate::sync::state::body_hash("remote edit"));

        let output = push_note(&fixture.client(), &files, stale)
            .await
            .expect("comparison should succeed")
            .expect("tracked note should resolve");

        // Only the one GET: the unchanged local file must never be written over
        // someone else's remote edit.
        assert_eq!(output.status, PushStatus::RemoteChanged);
        assert_eq!(fixture.finish().len(), 1);
    }

    #[tokio::test]
    async fn a_malformed_expected_hash_is_refused_before_any_request() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "merged").expect("local fixture should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let mut merged = input(&local_path, PushStrategy::Safe, false);
        merged.expected_remote_hash = Some("not-a-hash".to_owned());
        assert!(matches!(
            push_note(&client, &files, merged).await,
            Err(PushNoteError::MalformedExpectedHash)
        ));
    }

    #[tokio::test]
    async fn a_merge_is_not_pushed_once_the_remote_it_merged_has_moved() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline plus merged remote edit")
            .expect("local fixture should write");

        // The remote edit that was merged has since been reverted, so the
        // remote is back at the baseline and the file looks locally changed.
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            r#"{"id":"note-id","title":"Note","content":"baseline"}"#,
        )]);
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let mut merged = input(&local_path, PushStrategy::Safe, false);
        merged.expected_remote_hash = Some(crate::sync::state::body_hash("remote edit"));

        let output = push_note(&fixture.client(), &files, merged)
            .await
            .expect("comparison should succeed")
            .expect("tracked note should resolve");

        assert_eq!(output.status, PushStatus::Conflict);
        assert_eq!(
            output.remote_body_hash,
            Some(crate::sync::state::body_hash("baseline"))
        );
        assert_eq!(fixture.finish().len(), 1, "nothing may be written");
    }

    #[tokio::test]
    async fn identical_edits_on_both_sides_advance_the_baseline() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "same edit").expect("local fixture should write");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            r#"{"id":"note-id","title":"Note","content":"same edit","lastChangedAt":5}"#,
        )]);
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let output = push_note(
            &fixture.client(),
            &files,
            input(&local_path, PushStrategy::Safe, false),
        )
        .await
        .expect("comparison should succeed")
        .expect("tracked note should resolve");
        assert_eq!(output.status, PushStatus::NothingToPush);
        assert_eq!(
            files
                .state()
                .load_for_local_path(&local_path)
                .expect("advanced state should load")
                .baseline_body,
            "same edit"
        );
        fixture.finish();
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

    #[test]
    fn conflict_diff_bounds_maximum_size_single_lines_before_formatting() {
        let baseline = "a".repeat(BODY_MAX_BYTES);
        let local = "b".repeat(BODY_MAX_BYTES);
        let remote = "界".repeat(BODY_MAX_BYTES / "界".len());

        let summary = conflict_diff(&baseline, &local, &remote);

        assert!(summary.chars().count() <= 4_001);
        assert!(summary.contains("LOCAL CHANGES"));
        assert!(summary.contains("REMOTE CHANGES"));
        assert!(summary.contains("bytes omitted from diff input"));
        assert!(summary.ends_with('…'));
    }

    #[tokio::test]
    async fn unchanged_local_reports_remote_change_without_patch() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
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
        fixture.finish();
    }

    #[tokio::test]
    async fn unchanged_local_and_remote_is_a_no_op() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
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
        fixture.finish();
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
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"remote edit"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
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
        fixture.finish();
    }

    #[tokio::test]
    async fn a_bare_note_id_is_checked_in_the_tracked_workspace() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let files = crate::fixture::tracked_files_in(
            Workspace::Team {
                team_path: "core".to_owned(),
            },
            directory.path(),
            "note-id",
            &local_path,
            "baseline",
        );
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/teams/core/notes/note-id",
            200,
            r#"{"id":"note-id","title":"Note","content":"baseline"}"#,
        )]);
        let mut named = input(&local_path, PushStrategy::Safe, false);
        named.note_ref = Some("note-id".to_owned());

        let output = push_note(&fixture.client(), &files, named)
            .await
            .expect("a matching bare ID should pass the cross-check")
            .expect("tracked note should resolve");
        assert_eq!(output.status, PushStatus::NothingToPush);
        fixture.finish();
    }

    #[tokio::test]
    async fn a_note_ref_naming_another_note_is_refused() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "local edit").expect("local fixture should write");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let mut other = input(&local_path, PushStrategy::Safe, false);
        other.note_ref = Some("other-id".to_owned());
        assert!(matches!(
            push_note(&client, &files, other).await,
            Err(PushNoteError::TrackingMismatch)
        ));
    }
}
