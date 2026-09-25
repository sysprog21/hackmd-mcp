use std::path::{Path, PathBuf};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    local::{Entry, LocalAccessError, LocalFiles},
    models::Workspace,
    note::reference::{NoteRefError, NoteResolution},
    sync::state::{StateError, TrackedNoteState, body_hash},
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
}

#[derive(Debug, Error)]
pub(crate) enum PullNoteError {
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error("local_path must be absolute")]
    RelativePath,
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
    #[error("remote note {note_id} has no Markdown content")]
    MissingContent { note_id: String },
    #[error(transparent)]
    BodySize(#[from] BodySizeError),
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn pull_note(
    client: &HackmdClient,
    files: &LocalFiles,
    input: PullNoteInput,
) -> Result<Result<PullNoteOutput, NoteResolution>, PullNoteError> {
    if !input.local_path.is_absolute() {
        return Err(PullNoteError::RelativePath);
    }
    files.allow(&input.local_path)?;
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
    let remote = client.get_note(&note.workspace, &note.note_id).await?;
    let body = remote
        .content
        .ok_or_else(|| PullNoteError::MissingContent {
            note_id: note.note_id.clone(),
        })?;
    let size_bytes = body.len();
    check_body_size("remote body", size_bytes, input.confirm_large_file)?;

    // Checked again now that the note is in hand: the requests above take real
    // time, and an editor that saved into the file meanwhile must not lose that
    // save. Checking first as well keeps a doomed pull off the network.
    validate_destination(files, &input)?;
    files.write_atomic(&input.local_path, body.as_bytes(), input.create_parent_dirs)?;
    let destination = input
        .local_path
        .canonicalize()
        .map_err(LocalAccessError::Io)?;

    let state = TrackedNoteState::capture(
        note.note_id.clone(),
        note.workspace.clone(),
        destination.clone(),
        &body,
        remote.last_changed_at,
    )?;
    files.state().persist_from_sync(&state, &body)?;
    Ok(Ok(PullNoteOutput {
        workspace: note.workspace,
        note_id: note.note_id,
        title: remote.title,
        local_path: destination,
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
            if !input.discard_local_changes && has_unpushed_changes(files, input)? {
                return Err(PullNoteError::UnpushedLocalChanges);
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
/// not that, so it stops the pull rather than waving it on.
fn has_unpushed_changes(files: &LocalFiles, input: &PullNoteInput) -> Result<bool, PullNoteError> {
    let record = match files.state().record_for(&input.local_path) {
        Ok(Some(record)) => record,
        Err(error @ (StateError::Io(_) | StateError::InvalidStatePath)) => {
            return Err(error.into());
        }
        Ok(None) | Err(_) => return Ok(false),
    };

    // One byte past the maximum is enough to tell an oversized file apart, and
    // a body that is not UTF-8 was never a synced baseline.
    let local = files.read_capped(&input.local_path, BODY_MAX_BYTES + 1)?;
    Ok(std::str::from_utf8(&local)
        .map_or(true, |body| body_hash(body) != record.baseline_body_hash))
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
            Err(PullNoteError::RelativePath)
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

    #[tokio::test]
    async fn overwriting_unpushed_edits_needs_an_explicit_discard() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("local fixture should write");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let files =
            crate::fixture::tracked_files(directory.path(), "note-id", &local_path, "baseline");
        let pull = |discard_local_changes| PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            local_path: local_path.clone(),
            overwrite_local: true,
            discard_local_changes,
            create_parent_dirs: false,
            confirm_large_file: false,
        };

        // Unchanged since the last sync: the guard lets the pull through to the
        // network, which this client has no token for.
        assert!(matches!(
            pull_note(&client, &files, pull(false)).await,
            Err(PullNoteError::Api(_))
        ));

        fs::write(&local_path, "unpushed edit").expect("local edit should write");
        assert!(matches!(
            pull_note(&client, &files, pull(false)).await,
            Err(PullNoteError::UnpushedLocalChanges)
        ));
        assert!(matches!(
            pull_note(&client, &files, pull(true)).await,
            Err(PullNoteError::Api(_))
        ));
    }
}
