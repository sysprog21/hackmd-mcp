use std::path::{Path, PathBuf};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    hash::body_hash,
    local::{Entry, LocalAccessError, LocalFiles},
    models::Workspace,
    note::reference::{NoteRefError, NoteResolution},
    sync::state::{StateError, TrackedNoteState},
    sync::{BODY_MAX_BYTES, BodySizeError, check_body_size},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent MCP wire options must remain backward compatible"
)]
pub(crate) struct PullNoteInput {
    /// Team path (from `hackmd_get_me`) when a direct internal note ID belongs
    /// to a team; omit for personal notes. `@owner/slug` URLs name their own
    /// workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Internal API ID, `hackmd.io/<id>`, or `hackmd.io/@owner/slug` URL.
    pub(crate) note_ref: String,
    /// Bypass the 60-second account and note-list caches when resolving an
    /// `@owner/slug` URL.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// Absolute destination path ending in `.md` for the exact remote body.
    pub(crate) local_path: PathBuf,
    /// Replace an existing file at `local_path`.
    #[serde(default)]
    pub(crate) overwrite_local: bool,
    /// Also replace a tracked file whose edits were never pushed. Without it,
    /// such a pull is refused rather than silently losing those edits.
    #[serde(default)]
    pub(crate) discard_local_changes: bool,
    /// Create missing parent directories of `local_path`.
    #[serde(default)]
    pub(crate) create_parent_dirs: bool,
    /// Required to pull a body above 5 MiB.
    #[serde(default)]
    pub(crate) confirm_large_file: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct PullNoteOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    pub(crate) title: String,
    pub(crate) local_path: PathBuf,
    pub(crate) bytes: usize,
    /// The body hash now recorded as the baseline, the same `sha256:` form
    /// `hackmd_push_note` and `hackmd_get_note` report. It is the hash of the
    /// bytes just written locally, so a caller can confirm the file landed
    /// intact and anchor later verification without reading the sync sidecar.
    pub(crate) body_hash: String,
}

#[derive(Debug, Error)]
pub(crate) enum PullNoteError {
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error("local_path points to a directory")]
    DestinationDirectory,
    #[error("local_path already exists; retry with overwrite_local: true")]
    DestinationExists,
    #[error("local_path must name a Markdown file ending in .md")]
    NotMarkdown,
    #[error(
        "local_path has edits that were never pushed; push them first, or retry with discard_local_changes: true to lose them"
    )]
    UnpushedLocalChanges,
    #[error("destination parent does not exist; retry with create_parent_dirs: true")]
    MissingParent,
    #[error("destination parent is not a directory")]
    InvalidParent,
    #[error(transparent)]
    BodySize(#[from] BodySizeError),
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

impl crate::reply::ToolError for PullNoteError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::Access(error) => error.kind(),
            Self::DestinationDirectory | Self::NotMarkdown | Self::InvalidParent => {
                ErrorKind::InvalidInput
            }
            Self::DestinationExists | Self::MissingParent => ErrorKind::ConfirmationRequired,
            Self::UnpushedLocalChanges => ErrorKind::UnpushedChanges,
            Self::BodySize(error) => error.kind(),
            Self::Reference(error) => error.kind(),
            Self::State(error) => error.kind(),
            Self::Api(error) => error.kind(),
        }
    }
}

pub(crate) async fn pull_note(
    client: &HackmdClient,
    files: &LocalFiles,
    input: PullNoteInput,
) -> Result<Result<PullNoteOutput, NoteResolution>, PullNoteError> {
    files.allow_write(&input.local_path)?;

    // A store that must not be trusted is refused before the file is touched,
    // discard or not: the record this pull saves would be read back from there.
    files.state().trusted()?;
    validate_destination(files, &input)?;
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

    // Held from the fetch until the record is saved: a push finishing in
    // between would otherwise be overwritten by this older body.
    let _sync = files.sync_lock().await;
    let (remote, body) = client.get_note_body(&note.workspace, &note.note_id).await?;
    let size_bytes = body.len();
    check_body_size("remote body", size_bytes, input.confirm_large_file)?;

    // Checked again now that the note is in hand: the requests above take real
    // time, and an editor that saved into the file meanwhile must not lose that
    // save. Checking first as well keeps a doomed pull off the network. Only
    // this second check judges unpushed edits, because a file that already
    // holds exactly the remote body has nothing to lose, which is what a push
    // whose state could not be saved leaves behind.
    validate_destination(files, &input)?;
    if !input.discard_local_changes && has_unpushed_changes(files, &input, &body)? {
        return Err(PullNoteError::UnpushedLocalChanges);
    }
    files.write_atomic(&input.local_path, body.as_bytes(), input.create_parent_dirs)?;
    let state = crate::local::offload(|| {
        TrackedNoteState::capture(
            note.note_id.clone(),
            note.workspace.clone(),
            &input.local_path,
            &body,
            remote.last_changed_at,
        )
    })?;
    files.state().persist_from_sync(&state, &body)?;
    Ok(Ok(PullNoteOutput {
        workspace: note.workspace,
        note_id: note.note_id,
        title: remote.title,
        body_hash: state.baseline_body_hash,
        // The canonical path the record keeps, which is what push reports back.
        local_path: state.local_path,
        bytes: size_bytes,
    }))
}

fn validate_destination(files: &LocalFiles, input: &PullNoteInput) -> Result<(), PullNoteError> {
    // Only Markdown, so a note that talks an agent into pulling over a shell
    // profile or an SSH key file gets an error instead.
    if !is_markdown(&input.local_path) {
        return Err(PullNoteError::NotMarkdown);
    }
    match files.entry(&input.local_path)? {
        Some(Entry::Directory) => return Err(PullNoteError::DestinationDirectory),
        Some(Entry::Other) => {
            if !input.overwrite_local {
                return Err(PullNoteError::DestinationExists);
            }
            return Ok(());
        }
        None => {}
    }
    let parent = input
        .local_path
        .parent()
        .ok_or(PullNoteError::InvalidParent)?;
    match files.entry(parent)? {
        Some(Entry::Directory) => Ok(()),
        Some(Entry::Other) => Err(PullNoteError::InvalidParent),
        None if input.create_parent_dirs => Ok(()),
        None => Err(PullNoteError::MissingParent),
    }
}

/// Whether a tracked destination differs from the body last synced to it,
/// judged against the hash in its record, so no baseline is read. An untracked
/// file, or one whose record is unreadable, has nothing to lose edits against:
/// re-pulling is exactly how a broken record is repaired. An I/O failure is
/// not that, so it stops the pull rather than waving it on. Nor does a file
/// already identical to `remote`: the pull would rewrite the same bytes.
fn has_unpushed_changes(
    files: &LocalFiles,
    input: &PullNoteInput,
    remote: &str,
) -> Result<bool, PullNoteError> {
    // Two records naming the file are still tracking it, but there is no single
    // baseline to judge against: only a file already equal to the remote is
    // known to have nothing to lose. Only an absent or unusable record means
    // there is nothing to lose; any other failure, now or added later, refuses
    // rather than overwrite.
    let baseline_hash = match files.state().record_for(&input.local_path) {
        Ok(Some(record)) => Some(record.baseline_body_hash),
        Err(StateError::AmbiguousTrackedState { .. }) => None,
        Ok(None)
        | Err(
            StateError::NotTracked
            | StateError::CorruptTrackedState { .. }
            | StateError::StateIdentityMismatch
            | StateError::MissingBaseline { .. }
            | StateError::BaselineMismatch { .. },
        ) => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    // One byte past the maximum is enough to tell an oversized file apart, and
    // a body that is not UTF-8 was never a synced baseline.
    let local = files.read_capped(&input.local_path, BODY_MAX_BYTES + 1)?;
    Ok(crate::local::offload(|| {
        std::str::from_utf8(&local).map_or(true, |body| {
            body != remote
                && baseline_hash
                    .as_deref()
                    .is_none_or(|baseline| body_hash(body) != baseline)
        })
    }))
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::{PullNoteError, PullNoteInput, pull_note};
    use crate::{client::HackmdClient, config::Config, local::LocalFiles, models::Workspace};

    #[tokio::test]
    async fn a_workspace_root_stops_a_write_outside_it() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let root = directory.path().join("notes");
        fs::create_dir(&root).expect("root should create");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let files = LocalFiles::new(directory.path().join("state"), Some(root));
        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            local_path: directory.path().join("outside.md"),
            overwrite_local: true,
            discard_local_changes: false,
            create_parent_dirs: true,
            confirm_large_file: false,
        };

        // Refused before the note is even resolved, so no request is made and
        // nothing is written.
        assert!(matches!(
            pull_note(&client, &files, input).await,
            Err(PullNoteError::Access(_))
        ));
    }

    #[tokio::test]
    async fn clean_pull_writes_exact_body_and_private_sync_state() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let destination = directory.path().join("nested/note.md");
        let state_dir = directory.path().join("state");
        let fixture = crate::fixture::SequenceServer::spawn_scenarios([
            crate::fixture::Scenario::new(
                "GET",
                "/v1/notes/note%2Fid",
                200,
                r##"{"id":"note/id","title":"Remote","content":"# Exact\n\nBody\n","lastChangedAt":123}"##,
            ),
        ]);
        let client = fixture.client();
        let files = crate::fixture::unconfined_files(state_dir.clone());
        let output = pull_note(
            &client,
            &files,
            PullNoteInput {
                workspace: Workspace::Personal,
                note_ref: "note/id".to_owned(),
                refresh: false,
                local_path: destination.clone(),
                overwrite_local: false,
                discard_local_changes: false,
                create_parent_dirs: true,
                confirm_large_file: false,
            },
        )
        .await
        .expect("pull should succeed")
        .expect("direct note should resolve");

        // The tool reports the canonical destination: on macOS the temporary
        // directory itself lives behind a /var -> /private/var symlink.
        assert_eq!(
            output.local_path,
            destination
                .canonicalize()
                .expect("destination should exist")
        );
        assert_eq!(
            fs::read_to_string(&destination).expect("note should read"),
            "# Exact\n\nBody\n"
        );
        assert_eq!(
            output.body_hash,
            crate::hash::body_hash("# Exact\n\nBody\n"),
            "pull reports the hash of the exact bytes it wrote"
        );
        let tracked = state_dir.join("tracked");
        let entries = fs::read_dir(&tracked)
            .expect("tracked state should exist")
            .map(|entry| entry.expect("entry should read").path())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 2);
        let baseline = entries
            .iter()
            .find(|path| path.extension().is_some_and(|extension| extension == "md"))
            .expect("baseline should exist");
        assert_eq!(
            fs::read_to_string(baseline).expect("baseline should read"),
            "# Exact\n\nBody\n"
        );
        let sidecar = entries
            .iter()
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .expect("sidecar should exist");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(sidecar).expect("sidecar should read"))
                .expect("sidecar should be JSON");
        assert_eq!(value["internal_id"], "note/id");
        // Sync state keeps the tagged form older builds read.
        assert_eq!(value["workspace"], json!({"kind": "personal"}));
        assert_eq!(value["last_observed_remote_timestamp"], "123");
        assert!(
            value["baseline_body_hash"]
                .as_str()
                .expect("hash should be text")
                .starts_with("sha256:")
        );
        fixture.finish();
    }

    /// Manual peak-memory workload used by `scripts/measure-memory.sh`.
    #[tokio::test]
    #[ignore = "manual 10 MiB pull memory baseline"]
    async fn benchmark_10_mib_pull() {
        const WORKLOAD_BYTES: usize = 10 * 1024 * 1024;
        let directory = tempfile::tempdir().expect("temp directory should create");
        let destination = directory.path().join("note.md");
        let content = "x".repeat(WORKLOAD_BYTES);
        let response = serde_json::to_string(&serde_json::json!({
            "id": "large-note",
            "title": "Large",
            "content": content,
            "lastChangedAt": 1
        }))
        .expect("large response should serialize");
        let fixture =
            crate::fixture::SequenceServer::spawn_scenarios([crate::fixture::Scenario::new(
                "GET",
                "/v1/notes/large-note",
                200,
                &response,
            )]);
        let files = crate::fixture::unconfined_files(directory.path().join("state"));
        let output = pull_note(
            &fixture.client(),
            &files,
            PullNoteInput {
                workspace: Workspace::Personal,
                note_ref: "large-note".to_owned(),
                refresh: false,
                local_path: destination.clone(),
                overwrite_local: false,
                discard_local_changes: false,
                create_parent_dirs: true,
                confirm_large_file: true,
            },
        )
        .await
        .expect("maximum-size pull should succeed")
        .expect("direct note should resolve");
        assert_eq!(output.bytes, WORKLOAD_BYTES);
        assert_eq!(
            fs::metadata(destination).expect("note should exist").len(),
            WORKLOAD_BYTES as u64
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn path_guards_fail_before_network_or_filesystem_mutation() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let files = crate::fixture::scratch_files();
        let relative = serde_json::from_value(json!({
            "note_ref": "id",
            "local_path": "note.md"
        }))
        .expect("input should deserialize");
        assert!(matches!(
            pull_note(&client, &files, relative).await,
            Err(PullNoteError::Access(
                crate::local::LocalAccessError::Relative { .. }
            ))
        ));

        let directory = tempfile::tempdir().expect("temp directory should create");
        let existing = directory.path().join("note.md");
        fs::write(&existing, "local").expect("fixture should write");
        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            refresh: false,
            local_path: existing,
            overwrite_local: false,
            discard_local_changes: false,
            create_parent_dirs: false,
            confirm_large_file: false,
        };
        assert!(matches!(
            pull_note(&client, &files, input).await,
            Err(PullNoteError::DestinationExists)
        ));

        let non_markdown = directory.path().join("note.txt");
        fs::write(&non_markdown, "local").expect("fixture should write");
        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            refresh: false,
            local_path: non_markdown,
            overwrite_local: false,
            discard_local_changes: false,
            create_parent_dirs: false,
            confirm_large_file: false,
        };
        assert!(matches!(
            pull_note(&client, &files, input).await,
            Err(PullNoteError::NotMarkdown)
        ));

        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            refresh: false,
            local_path: {
                let folder = directory.path().join("folder.md");
                fs::create_dir(&folder).expect("directory fixture should create");
                folder
            },
            overwrite_local: true,
            discard_local_changes: false,
            create_parent_dirs: false,
            confirm_large_file: false,
        };
        assert!(matches!(
            pull_note(&client, &files, input).await,
            Err(PullNoteError::DestinationDirectory)
        ));

        let missing_parent = directory.path().join("missing/note.md");
        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            refresh: false,
            local_path: missing_parent.clone(),
            overwrite_local: false,
            discard_local_changes: false,
            create_parent_dirs: false,
            confirm_large_file: false,
        };
        assert!(matches!(
            pull_note(&client, &files, input).await,
            Err(PullNoteError::MissingParent)
        ));
        assert!(
            !missing_parent
                .parent()
                .expect("parent should exist lexically")
                .exists()
        );
    }

    #[tokio::test]
    async fn an_edit_saved_while_the_note_is_fetched_is_not_overwritten() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let editor_path = local_path.clone();
        let fixture =
            crate::fixture::SequenceServer::spawn_scenarios([crate::fixture::Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"remote"}"#,
            )
            // An editor saving while the request is in flight.
            .before_response(move || {
                fs::write(&editor_path, "saved mid-pull").expect("editor save should write");
            })]);
        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            local_path: local_path.clone(),
            overwrite_local: true,
            discard_local_changes: false,
            create_parent_dirs: false,
            confirm_large_file: false,
        };

        let result = pull_note(&fixture.client(), &files, input).await;
        assert!(
            matches!(result, Err(PullNoteError::UnpushedLocalChanges)),
            "{result:?}"
        );
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should read"),
            "saved mid-pull"
        );
        fixture.finish();
    }

    fn overwrite_input(local_path: &std::path::Path, discard_local_changes: bool) -> PullNoteInput {
        PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            local_path: local_path.to_path_buf(),
            overwrite_local: true,
            discard_local_changes,
            create_parent_dirs: false,
            confirm_large_file: false,
        }
    }

    fn remote_note(body: &str) -> crate::fixture::Scenario {
        crate::fixture::Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            &json!({"id": "note-id", "title": "Note", "content": body}).to_string(),
        )
    }

    #[tokio::test]
    async fn overwriting_unpushed_edits_needs_an_explicit_discard() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "unpushed edit").expect("local edit should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");

        let fixture = crate::fixture::SequenceServer::spawn_scenarios([remote_note("remote")]);
        assert!(matches!(
            pull_note(
                &fixture.client(),
                &files,
                overwrite_input(&local_path, false)
            )
            .await,
            Err(PullNoteError::UnpushedLocalChanges)
        ));
        fixture.finish();
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should read"),
            "unpushed edit"
        );

        let fixture = crate::fixture::SequenceServer::spawn_scenarios([remote_note("remote")]);
        pull_note(
            &fixture.client(),
            &files,
            overwrite_input(&local_path, true),
        )
        .await
        .expect("discarding pull should succeed")
        .expect("note should resolve");
        fixture.finish();
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should read"),
            "remote"
        );
    }

    /// A store that must not be trusted cannot vouch that a file has no
    /// unpushed edits, so the pull refuses rather than overwrite it.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_untrusted_state_directory_never_lets_a_pull_overwrite() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "unpushed edit").expect("local edit should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        fs::set_permissions(
            directory.path().join("state"),
            fs::Permissions::from_mode(0o777),
        )
        .expect("permissions should set");

        let fixture = crate::fixture::SequenceServer::spawn_scenarios([remote_note("remote")]);
        assert!(matches!(
            pull_note(
                &fixture.client(),
                &files,
                overwrite_input(&local_path, false)
            )
            .await,
            Err(PullNoteError::State(
                crate::sync::state::StateError::UntrustedStateDir { .. }
            ))
        ));
        // Not even when told to discard local changes.
        assert!(matches!(
            pull_note(
                &fixture.client(),
                &files,
                overwrite_input(&local_path, true)
            )
            .await,
            Err(PullNoteError::State(
                crate::sync::state::StateError::UntrustedStateDir { .. }
            ))
        ));
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should read"),
            "unpushed edit"
        );
    }

    #[tokio::test]
    async fn a_push_whose_state_was_lost_is_repaired_by_a_plain_pull() {
        // What a push leaves when HackMD took the write but the sidecar could
        // not be saved: the file and the remote agree, the record does not.
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "pushed body").expect("local fixture should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");

        let fixture = crate::fixture::SequenceServer::spawn_scenarios([remote_note("pushed body")]);
        pull_note(
            &fixture.client(),
            &files,
            overwrite_input(&local_path, false),
        )
        .await
        .expect("pull should repair the record without discard_local_changes")
        .expect("note should resolve");
        fixture.finish();
        assert_eq!(
            files
                .state()
                .load_for_local_path(&local_path)
                .expect("state should load")
                .baseline_body,
            "pushed body"
        );
    }

    #[tokio::test]
    async fn a_file_with_two_records_still_guards_its_unpushed_edits() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "unpushed edit").expect("local edit should write");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        // A second record for the same file, and no pointer to settle it.
        let state = directory.path().join("state");
        fs::copy(
            state.join("tracked/personal--note%2Did.json"),
            state.join("tracked/personal--other.json"),
        )
        .expect("sidecar should copy");
        fs::remove_dir_all(state.join("by-path")).expect("index should be removable");

        let fixture = crate::fixture::SequenceServer::spawn_scenarios([remote_note("remote")]);
        assert!(matches!(
            pull_note(
                &fixture.client(),
                &files,
                overwrite_input(&local_path, false)
            )
            .await,
            Err(PullNoteError::UnpushedLocalChanges)
        ));
        fixture.finish();
        assert_eq!(
            fs::read_to_string(&local_path).expect("local should read"),
            "unpushed edit"
        );
    }
}
