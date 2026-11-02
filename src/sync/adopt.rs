use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

use crate::{
    client::HackmdClient,
    local::{LocalAccessError, LocalFiles},
    note::crud::{CreateNoteInput, CreateNoteOutput, CrudError, create_note},
    sync::{
        LocalBodyError,
        pull::is_markdown,
        read_local_body,
        state::{StateError, TrackedNoteState},
    },
};

#[derive(Debug, Error)]
pub(crate) enum AdoptError {
    #[error("local_path replaces content; give one of them")]
    ContentAndPath,
    #[error("local_path must name a Markdown file ending in .md")]
    NotMarkdown,
    #[error(
        "publishing a local file needs an explicit read_permission: HackMD's default for an omitted one is unmeasured and may be more open than intended"
    )]
    ReadPermissionRequired,
    #[error(
        "local_path is already tracked for note {note_id}; hackmd_push_note updates it, or untrack it first to publish it as a new note"
    )]
    AlreadyTracked { note_id: String },
    #[error(transparent)]
    Access(#[from] LocalAccessError),
    #[error(transparent)]
    Body(#[from] LocalBodyError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Create(#[from] CrudError),
}

impl crate::reply::ToolError for AdoptError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::ContentAndPath | Self::NotMarkdown | Self::ReadPermissionRequired => {
                ErrorKind::InvalidInput
            }
            Self::AlreadyTracked { .. } => ErrorKind::Conflict,
            Self::Access(error) => error.kind(),
            Self::Body(error) => error.kind(),
            Self::State(error) => error.kind(),
            Self::Create(error) => error.kind(),
        }
    }
}

/// A file may be published only where it may also be written, because tracking
/// lets a later pull replace it. `allow_publish` already refuses agent
/// instruction files, by requiring a root the user chose.
fn allow_tracked_file(files: &LocalFiles, path: &Path) -> Result<(), LocalAccessError> {
    files.allow_publish(path)?;
    files.allow_write(path)
}

/// What `hackmd_create_note` returns: the created note, plus `tracking` when a
/// `local_path` was given.
#[derive(Debug, Serialize)]
pub(crate) struct CreatedNote {
    #[serde(flatten)]
    pub(crate) created: CreateNoteOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tracking: Option<Tracking>,
}

/// Whether the published file is now tracked. A failure here is never an
/// error: the note exists, and an error would invite a second create.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum Tracking {
    Tracked {
        local_path: PathBuf,
        body_hash: String,
    },
    NotTracked {
        reason: String,
    },
}

/// `hackmd_create_note`, publishing the file at `input.local_path` when given.
/// Every refusal comes before the POST.
pub(crate) async fn create_note_from_file(
    client: &HackmdClient,
    files: &LocalFiles,
    mut input: CreateNoteInput,
) -> Result<CreatedNote, AdoptError> {
    let Some(path) = input.local_path.take() else {
        return Ok(CreatedNote {
            created: create_note(client, input).await?,
            tracking: None,
        });
    };
    if input.content.is_some() {
        return Err(AdoptError::ContentAndPath);
    }
    if input.read_permission.is_none() {
        return Err(AdoptError::ReadPermissionRequired);
    }
    allow_tracked_file(files, &path)?;

    // Everything below uses the resolved path, checked again, so a symlink
    // swapped in during the network calls cannot move the record to another
    // file.
    let path = crate::local::offload(|| std::fs::canonicalize(&path)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            LocalBodyError::NotAFile
        } else {
            LocalAccessError::Io(error).into()
        }
    })?;
    allow_tracked_file(files, &path)?;
    if !is_markdown(&path) {
        return Err(AdoptError::NotMarkdown);
    }
    files.state().trusted()?;

    // Held from the check for an existing record until the new one is saved, as
    // pull and push hold it, so two publishes of one file cannot both win.
    let _sync = files.sync_lock().await;
    match files.state().load_for_local_path(&path) {
        Err(StateError::NotTracked) => {}
        Ok(tracked) => {
            return Err(AdoptError::AlreadyTracked {
                note_id: tracked.state.internal_id,
            });
        }
        Err(error) => return Err(error.into()),
    }
    let body = read_local_body(files, &path, input.confirm_large_file)?;
    input.content = Some(body.clone());
    let created = create_note(client, input).await?;
    let tracking = Some(
        try_track(client, files, &path, &body, &created)
            .await
            .unwrap_or_else(|reason| Tracking::NotTracked { reason }),
    );
    Ok(CreatedNote { created, tracking })
}

/// Tracks the file only if the note reads back with exactly its body: the
/// baseline claims both sides hold it, and a claim that is wrong would show up
/// later as a remote change nobody made.
async fn try_track(
    client: &HackmdClient,
    files: &LocalFiles,
    path: &Path,
    body: &str,
    created: &CreateNoteOutput,
) -> Result<Tracking, String> {
    let note = &created.note;
    let readback = client
        .poll_readback(
            body.len(),
            || client.get_note_snapshot(&note.workspace, &note.id),
            |remote| remote.content.as_str() == body,
        )
        .await
        .map_err(|error| format!("the note could not be read back: {error}"))?;
    if !readback.confirmed {
        return Err(
            "the note did not read back with the file's body within the confirmation window; check the remote body before adopting it".into(),
        );
    }
    let save_failed = |error: StateError| {
        format!(
            "saving the sync record failed ({error}); it may be partly written, so check hackmd_list_notes with source tracked before relying on it"
        )
    };
    let state = crate::local::offload(|| {
        TrackedNoteState::capture(
            note.id.clone(),
            note.workspace.clone(),
            path,
            body,
            readback.value.last_changed_at,
        )
    })
    .map_err(save_failed)?;
    if state.local_path != path {
        return Err("the file was replaced while the note was created, so no sync record was written; check the remote body before adopting it".into());
    }
    files
        .state()
        .persist_from_sync(&state, body)
        .map_err(save_failed)?;
    Ok(Tracking::Tracked {
        local_path: state.local_path,
        body_hash: state.baseline_body_hash,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::{AdoptError, Tracking, create_note_from_file};
    use crate::{
        fixture::{Scenario, SequenceServer, confined_files, unconfined_files},
        note::crud::CreateNoteInput,
    };

    fn input(value: serde_json::Value) -> CreateNoteInput {
        serde_json::from_value(value).expect("create input should deserialize")
    }

    fn publish(file: &std::path::Path) -> CreateNoteInput {
        input(json!({"local_path": file, "read_permission": "owner"}))
    }

    fn created_then_read(stored: &str) -> [Scenario; 2] {
        [
            Scenario::new("POST", "/v1/notes", 201, r#"{"id":"new-id","title":"T","readPermission":"owner"}"#)
                .expect_body("the file's body, LF only", |body| {
                    body.contains(r##""content":"# T\nbody\n""##)
                }),
            Scenario::new(
                "GET",
                "/v1/notes/new-id",
                200,
                &json!({"id": "new-id", "title": "T", "readPermission": "owner", "content": stored, "lastChangedAt": 7})
                    .to_string(),
            ),
        ]
    }

    #[tokio::test]
    async fn a_published_file_is_tracked_once_the_note_reads_back_equal() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let file = directory.path().join("note.md");
        fs::write(&file, "# T\r\nbody\r\n").expect("file should write");
        let server = SequenceServer::spawn_scenarios(created_then_read("# T\nbody\n"));
        let files = confined_files(directory.path());
        let client = server.client();

        let output = create_note_from_file(&client, &files, publish(&file))
            .await
            .expect("publish should succeed");
        let Some(Tracking::Tracked { body_hash, .. }) = output.tracking else {
            panic!("a body that reads back equal should be tracked");
        };
        assert_eq!(body_hash, crate::hash::body_hash("# T\nbody\n"));

        // The file now has a record, so a second publish is refused before any
        // request is made.
        let again = create_note_from_file(&client, &files, publish(&file)).await;
        assert!(matches!(
            again,
            Err(AdoptError::AlreadyTracked { note_id }) if note_id == "new-id"
        ));
        server.finish();
    }

    #[tokio::test]
    async fn a_body_that_reads_back_different_is_left_untracked() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let file = directory.path().join("note.md");
        fs::write(&file, "# T\nbody\n").expect("file should write");
        let server = SequenceServer::spawn_repeating([
            (
                201,
                r#"{"id":"new-id","title":"T","readPermission":"owner"}"#,
            ),
            (
                200,
                r##"{"id":"new-id","title":"T","readPermission":"owner","content":"# T\nbody","lastChangedAt":7}"##,
            ),
        ]);
        let files = confined_files(directory.path());

        let output = create_note_from_file(&server.client(), &files, publish(&file))
            .await
            .expect("the note exists, so this is not an error");
        assert!(matches!(output.tracking, Some(Tracking::NotTracked { .. })));
        assert!(!directory.path().join("state/tracked").exists());
    }

    #[tokio::test]
    async fn refusals_come_before_any_request() {
        let directory = tempfile::tempdir().expect("temp directory should create");
        let text = directory.path().join("note.txt");
        fs::write(&text, "x").expect("file should write");
        let markdown = directory.path().join("note.md");
        fs::write(&markdown, "x").expect("file should write");
        let server = SequenceServer::spawn_scenarios([]);
        let files = confined_files(directory.path());
        let client = server.client();

        assert!(matches!(
            create_note_from_file(
                &client,
                &files,
                input(json!({"local_path": text, "read_permission": "owner"}))
            )
            .await,
            Err(AdoptError::NotMarkdown)
        ));
        assert!(matches!(
            create_note_from_file(&client, &files, input(json!({"local_path": markdown}))).await,
            Err(AdoptError::ReadPermissionRequired)
        ));
        assert!(matches!(
            create_note_from_file(
                &client,
                &files,
                input(json!({"local_path": markdown, "content": "y", "read_permission": "owner"}))
            )
            .await,
            Err(AdoptError::ContentAndPath)
        ));
        server.finish();
    }

    #[tokio::test]
    async fn publishing_requires_a_trusted_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("note.md");
        fs::write(&file, "secret").unwrap();
        let server = SequenceServer::spawn_scenarios([]);
        for files in [
            unconfined_files(directory.path().join("state")),
            confined_files(directory.path()).guarding_instructions(),
        ] {
            assert!(matches!(
                create_note_from_file(&server.client(), &files, publish(&file)).await,
                Err(AdoptError::Access(_))
            ));
        }
        server.finish();
    }

    #[tokio::test]
    async fn waits_for_the_created_body_to_be_visible() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("note.md");
        fs::write(&file, "# T\nbody\n").unwrap();
        let [post, stale] = created_then_read("stale");
        let [_, current] = created_then_read("# T\nbody\n");
        let server = SequenceServer::spawn_scenarios([post, stale, current]);
        let files = confined_files(directory.path());
        let output = create_note_from_file(&server.client(), &files, publish(&file))
            .await
            .unwrap();
        assert!(matches!(output.tracking, Some(Tracking::Tracked { .. })));
        server.finish();
    }

    #[tokio::test]
    async fn create_note_itself_refuses_a_local_path() {
        let server = SequenceServer::spawn_scenarios([]);
        let result = crate::note::crud::create_note(
            &server.client(),
            input(json!({"local_path": "/x/note.md"})),
        )
        .await;
        assert!(matches!(
            result,
            Err(crate::note::crud::CrudError::LocalPathNotHandled)
        ));
        server.finish();
    }

    #[test]
    fn every_variant_keeps_its_kind() {
        use crate::{
            local::LocalAccessError,
            note::crud::CrudError,
            reply::{ErrorKind, ToolError},
            sync::{LocalBodyError, state::StateError},
        };

        let path = std::path::PathBuf::from("note.md");
        let cases = [
            (AdoptError::ContentAndPath, ErrorKind::InvalidInput),
            (AdoptError::NotMarkdown, ErrorKind::InvalidInput),
            (AdoptError::ReadPermissionRequired, ErrorKind::InvalidInput),
            (
                AdoptError::AlreadyTracked {
                    note_id: "n".into(),
                },
                ErrorKind::Conflict,
            ),
            (
                AdoptError::Access(LocalAccessError::Relative { path }),
                ErrorKind::InvalidInput,
            ),
            (
                AdoptError::Body(LocalBodyError::NotAFile),
                ErrorKind::InvalidInput,
            ),
            (
                AdoptError::State(StateError::NotTracked),
                StateError::NotTracked.kind(),
            ),
            (
                AdoptError::Create(CrudError::LocalPathNotHandled),
                ErrorKind::Internal,
            ),
        ];
        for (error, kind) in cases {
            assert_eq!(error.kind(), kind, "{error}");
        }
    }

    #[tokio::test]
    async fn a_missing_file_is_not_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let server = SequenceServer::spawn_scenarios([]);
        let files = confined_files(directory.path());

        let result = create_note_from_file(
            &server.client(),
            &files,
            publish(&directory.path().join("absent.md")),
        )
        .await;
        assert!(matches!(
            result,
            Err(AdoptError::Body(crate::sync::LocalBodyError::NotAFile))
        ));
        server.finish();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_path_that_cannot_be_resolved_reports_the_access_error() {
        let directory = tempfile::tempdir().unwrap();
        let looped = directory.path().join("loop.md");
        std::os::unix::fs::symlink(&looped, &looped).unwrap();
        let server = SequenceServer::spawn_scenarios([]);
        let files = confined_files(directory.path());

        let result = create_note_from_file(&server.client(), &files, publish(&looped)).await;
        assert!(
            matches!(
                result,
                Err(AdoptError::Body(crate::sync::LocalBodyError::Access(
                    crate::local::LocalAccessError::Io(_)
                )))
            ),
            "{result:?}"
        );
        server.finish();
    }
}
