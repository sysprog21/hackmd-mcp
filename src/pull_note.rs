use std::{
    fs,
    path::{Path, PathBuf},
};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    models::Workspace,
    note_ref::{NoteRefError, NoteResolution},
    state::{StateError, TrackedNoteState, local_file_identity, write_local_atomic},
};

const WARNING_BYTES: usize = 5 * 1024 * 1024;
const MAX_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PullNoteInput {
    #[serde(default)]
    pub(crate) workspace: Workspace,
    pub(crate) note_ref: String,
    /// Absolute destination path for the exact remote Markdown body.
    pub(crate) local_path: PathBuf,
    #[serde(default)]
    pub(crate) overwrite_local: bool,
    #[serde(default)]
    pub(crate) create_parent_dirs: bool,
    #[serde(default)]
    pub(crate) confirm_large_file: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct PullNoteOutput {
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    pub(crate) title: String,
    pub(crate) local_path: PathBuf,
    pub(crate) bytes: usize,
}

#[derive(Debug, Error)]
pub(crate) enum PullNoteError {
    #[error("local_path must be absolute")]
    RelativePath,
    #[error("local_path points to a directory")]
    DestinationDirectory,
    #[error("local_path already exists; retry with overwrite_local: true")]
    DestinationExists,
    #[error("existing non-Markdown destination requires overwrite_local: true")]
    ExistingNonMarkdown,
    #[error("destination parent does not exist; retry with create_parent_dirs: true")]
    MissingParent,
    #[error("destination parent is not a directory")]
    InvalidParent,
    #[error("remote note {note_id} has no Markdown content")]
    MissingContent { note_id: String },
    #[error("remote body is {size_bytes} bytes; retry with confirm_large_file: true")]
    ConfirmationRequired { size_bytes: usize },
    #[error("remote body is {size_bytes} bytes; bodies above 50 MiB are refused")]
    TooLarge { size_bytes: usize },
    #[error("local path validation failed")]
    PathIo(#[source] std::io::Error),
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn pull_note(
    client: &HackmdClient,
    input: PullNoteInput,
) -> Result<Result<PullNoteOutput, NoteResolution>, PullNoteError> {
    let destination = validate_destination(&input)?;
    let allow_existing_destination = destination.exists() && input.overwrite_local;
    let resolution =
        crate::note_ref::resolve_note_ref(client, input.workspace, &input.note_ref).await?;
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
    if size_bytes > MAX_BYTES {
        return Err(PullNoteError::TooLarge { size_bytes });
    }
    if size_bytes > WARNING_BYTES && !input.confirm_large_file {
        return Err(PullNoteError::ConfirmationRequired { size_bytes });
    }
    let destination = prepare_destination(
        &destination,
        input.create_parent_dirs,
        allow_existing_destination,
    )?;
    write_local_atomic(&destination, body.as_bytes())?;
    let identity = local_file_identity(&destination)?;
    let state = TrackedNoteState {
        internal_id: note.note_id.clone(),
        workspace: note.workspace.clone(),
        local_path: destination.clone(),
        baseline_body_hash: format!("sha256:{:x}", Sha256::digest(body.as_bytes())),
        last_observed_remote_timestamp: remote
            .last_changed_at
            .map_or_else(String::new, |timestamp| timestamp.to_string()),
        local_file_identity: identity,
    };
    client.state().persist_from_sync(&state, &body)?;
    Ok(Ok(PullNoteOutput {
        workspace: note.workspace,
        note_id: note.note_id,
        title: remote.title,
        local_path: destination,
        bytes: size_bytes,
    }))
}

fn validate_destination(input: &PullNoteInput) -> Result<PathBuf, PullNoteError> {
    if !input.local_path.is_absolute() {
        return Err(PullNoteError::RelativePath);
    }
    if input.local_path.exists() {
        let metadata = fs::metadata(&input.local_path).map_err(PullNoteError::PathIo)?;
        if metadata.is_dir() {
            return Err(PullNoteError::DestinationDirectory);
        }
        if !input.overwrite_local {
            if !is_markdown(&input.local_path) {
                return Err(PullNoteError::ExistingNonMarkdown);
            }
            return Err(PullNoteError::DestinationExists);
        }
        return fs::canonicalize(&input.local_path).map_err(PullNoteError::PathIo);
    }
    let parent = input
        .local_path
        .parent()
        .ok_or(PullNoteError::InvalidParent)?;
    if !parent.exists() {
        if !input.create_parent_dirs {
            return Err(PullNoteError::MissingParent);
        }
        return Ok(input.local_path.clone());
    }
    if !parent.is_dir() {
        return Err(PullNoteError::InvalidParent);
    }
    let canonical_parent = fs::canonicalize(parent).map_err(PullNoteError::PathIo)?;
    let name = input
        .local_path
        .file_name()
        .ok_or(PullNoteError::InvalidParent)?;
    Ok(canonical_parent.join(name))
}

fn prepare_destination(
    path: &Path,
    create_parent_dirs: bool,
    allow_existing: bool,
) -> Result<PathBuf, PullNoteError> {
    if path.exists() {
        return if allow_existing {
            Ok(path.to_path_buf())
        } else {
            Err(PullNoteError::DestinationExists)
        };
    }
    let parent = path.parent().ok_or(PullNoteError::InvalidParent)?;
    if !parent.exists() {
        if !create_parent_dirs {
            return Err(PullNoteError::MissingParent);
        }
        fs::create_dir_all(parent).map_err(PullNoteError::PathIo)?;
    }
    let canonical_parent = fs::canonicalize(parent).map_err(PullNoteError::PathIo)?;
    let name = path.file_name().ok_or(PullNoteError::InvalidParent)?;
    Ok(canonical_parent.join(name))
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
    use crate::{client::HackmdClient, config::Config, models::Workspace};

    #[tokio::test]
    async fn clean_pull_writes_exact_body_and_private_sync_state() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let destination = directory.path().join("nested/note.md");
        let state_dir = directory.path().join("state");
        let fixture = crate::test_support::SequenceServer::spawn([(
            200,
            r##"{"id":"note/id","title":"Remote","content":"# Exact\n\nBody\n","lastChangedAt":123}"##,
        )]);
        let client = HackmdClient::new(Config::for_loopback_test_with_state(
            &fixture.api_url,
            "fixture-token",
            &state_dir,
        ))
        .expect("fixture client should build");
        let output = pull_note(
            &client,
            PullNoteInput {
                workspace: Workspace::Personal,
                note_ref: "note/id".to_owned(),
                local_path: destination.clone(),
                overwrite_local: false,
                create_parent_dirs: true,
                confirm_large_file: false,
            },
        )
        .await
        .expect("pull should succeed")
        .expect("direct note should resolve");
        assert_eq!(output.local_path, destination);
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
        assert_eq!(value["last_observed_remote_timestamp"], "123");
        assert!(
            value["baseline_body_hash"]
                .as_str()
                .expect("hash should be text")
                .starts_with("sha256:")
        );
        let requests = fixture.finish();
        assert!(requests[0].starts_with("GET /v1/notes/note%2Fid HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn path_guards_fail_before_network_or_filesystem_mutation() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let relative = serde_json::from_value(json!({
            "note_ref": "id",
            "local_path": "note.md"
        }))
        .expect("input should deserialize");
        assert!(matches!(
            pull_note(&client, relative).await,
            Err(PullNoteError::RelativePath)
        ));

        let directory = tempfile::tempdir().expect("temp directory should create");
        let existing = directory.path().join("note.md");
        fs::write(&existing, "local").expect("fixture should write");
        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            local_path: existing,
            overwrite_local: false,
            create_parent_dirs: false,
            confirm_large_file: false,
        };
        assert!(matches!(
            pull_note(&client, input).await,
            Err(PullNoteError::DestinationExists)
        ));

        let non_markdown = directory.path().join("note.txt");
        fs::write(&non_markdown, "local").expect("fixture should write");
        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            local_path: non_markdown,
            overwrite_local: false,
            create_parent_dirs: false,
            confirm_large_file: false,
        };
        assert!(matches!(
            pull_note(&client, input).await,
            Err(PullNoteError::ExistingNonMarkdown)
        ));

        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            local_path: directory.path().to_path_buf(),
            overwrite_local: true,
            create_parent_dirs: false,
            confirm_large_file: false,
        };
        assert!(matches!(
            pull_note(&client, input).await,
            Err(PullNoteError::DestinationDirectory)
        ));

        let missing_parent = directory.path().join("missing/note.md");
        let input = PullNoteInput {
            workspace: Workspace::Personal,
            note_ref: "id".to_owned(),
            local_path: missing_parent.clone(),
            overwrite_local: false,
            create_parent_dirs: false,
            confirm_large_file: false,
        };
        assert!(matches!(
            pull_note(&client, input).await,
            Err(PullNoteError::MissingParent)
        ));
        assert!(
            !missing_parent
                .parent()
                .expect("parent should exist lexically")
                .exists()
        );
    }
}
