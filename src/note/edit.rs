use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    models::Workspace,
    note::patch::PatchError,
    note::reference::{NoteRefError, NoteResolution},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditNoteInput {
    /// Workspace used for a direct internal note ID; scoped URLs resolve their
    /// own workspace.
    #[serde(default)]
    pub(crate) workspace: Workspace,
    /// Internal API ID, `hackmd.io/<id>`, or `hackmd.io/@owner/slug` URL.
    pub(crate) note_ref: String,
    /// Bypass cached note lists when resolving an `@owner/slug` URL.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// Codex patch envelope targeting the exact `patch_path` from
    /// `hackmd_get_note`.
    pub(crate) patch: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct EditNoteOutput {
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    pub(crate) patch_path: String,
    pub(crate) changed: bool,
    pub(crate) content: String,
}

#[derive(Debug, Error)]
pub(crate) enum EditNoteError {
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    Api(#[from] HackmdError),
    #[error(transparent)]
    Patch(#[from] PatchError),
    #[error("GET returned note {note_id} without editable content")]
    MissingContent { note_id: String },
    #[error("HackMD accepted the edit for note {note_id}, but read-back content did not match")]
    ReadbackMismatch { note_id: String },
}

pub(crate) async fn edit_note(
    client: &HackmdClient,
    input: EditNoteInput,
) -> Result<Result<EditNoteOutput, NoteResolution>, EditNoteError> {
    let resolution = crate::note::reference::resolve_note_ref(
        client,
        input.workspace,
        &input.note_ref,
        input.refresh,
    )
    .await?;
    let NoteResolution::Resolved { note } = resolution else {
        return Ok(Err(resolution));
    };
    let response = client.get_note(&note.workspace, &note.note_id).await?;
    let content = response
        .content
        .ok_or_else(|| EditNoteError::MissingContent {
            note_id: note.note_id.clone(),
        })?;
    let patch_path = crate::note::patch::patch_path(&note.workspace, &note.note_id);
    let updated = crate::note::patch::apply_note_patch(&content, &input.patch, &patch_path)?;
    let changed = updated != content;
    if changed {
        client
            .update_note_content(&note.workspace, &note.note_id, &updated)
            .await?;
        let readback = crate::client::poll_readback(
            || client.get_note(&note.workspace, &note.note_id),
            |readback| readback.content.as_deref() == Some(updated.as_str()),
        )
        .await?;
        if !readback.confirmed {
            return Err(EditNoteError::ReadbackMismatch {
                note_id: note.note_id,
            });
        }
    }
    Ok(Ok(EditNoteOutput {
        workspace: note.workspace,
        note_id: note.note_id,
        patch_path,
        changed,
        content: updated,
    }))
}

#[cfg(test)]
mod tests {
    use super::{EditNoteError, EditNoteInput, edit_note};
    use crate::{
        fixture::{SequenceServer, assert_request_sequence},
        models::Workspace,
        note::patch::PatchError,
    };

    fn input(patch: &str) -> EditNoteInput {
        EditNoteInput {
            workspace: Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            patch: patch.to_owned(),
        }
    }

    #[tokio::test]
    async fn a_write_that_never_becomes_visible_is_an_error() {
        const OLD: &str = r#"{"id":"note-id","title":"Title","content":"old"}"#;
        // Fetch, PATCH, then a read-back that keeps showing the old body.
        let fixture = SequenceServer::spawn_repeating([(200, OLD), (202, ""), (200, OLD)]);
        let client = fixture.client();
        let input = EditNoteInput {
            workspace: Workspace::Personal,
            note_ref: "note-id".to_owned(),
            refresh: false,
            patch:
                "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch"
                    .to_owned(),
        };

        assert!(matches!(
            edit_note(&client, input).await,
            Err(EditNoteError::ReadbackMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn changed_edit_gets_then_patches_full_updated_content() {
        let server = SequenceServer::spawn([
            (200, r#"{"id":"note-id","title":"Title","content":"old\n"}"#),
            (202, ""),
            (200, r#"{"id":"note-id","title":"Title","content":"new\n"}"#),
        ]);
        let patch =
            "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch";
        let output = edit_note(&server.client(), input(patch))
            .await
            .expect("edit should succeed")
            .expect("reference should resolve");
        assert!(output.changed);
        assert_eq!(output.content, "new\n");
        let requests = server.finish();
        assert_request_sequence(
            &requests,
            &[
                "GET /v1/notes/note-id HTTP/1.1",
                "PATCH /v1/notes/note-id HTTP/1.1",
                "GET /v1/notes/note-id HTTP/1.1",
            ],
        );
        assert!(requests[1].ends_with(r#"{"content":"new\n"}"#));
    }

    #[tokio::test]
    async fn no_op_edit_gets_once_and_never_patches() {
        let server =
            SequenceServer::spawn([(200, r#"{"id":"note-id","title":"Title","content":"same"}"#)]);
        let patch = "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n same\n*** End Patch";
        let output = edit_note(&server.client(), input(patch))
            .await
            .expect("no-op should succeed")
            .expect("reference should resolve");
        assert!(!output.changed);
        assert_request_sequence(&server.finish(), &["GET /v1/notes/note-id HTTP/1.1"]);
    }

    #[tokio::test]
    async fn patch_conflict_returns_distinct_error_without_patch_request() {
        let server = SequenceServer::spawn([(
            200,
            r#"{"id":"note-id","title":"Title","content":"actual"}"#,
        )]);
        let patch =
            "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-missing\n+new\n*** End Patch";
        let error = edit_note(&server.client(), input(patch))
            .await
            .expect_err("missing context should conflict");
        assert!(matches!(
            error,
            EditNoteError::Patch(PatchError::ContextNotFound)
        ));
        assert_eq!(server.finish().len(), 1);
    }

    #[tokio::test]
    async fn wrong_target_and_ambiguous_context_also_never_patch() {
        let cases = [
            (
                r#"{"id":"note-id","title":"Title","content":"old"}"#,
                "*** Begin Patch\n*** Update File: notes/other.md\n@@\n-old\n+new\n*** End Patch",
                "patch targets notes/other.md, expected notes/note-id.md",
            ),
            (
                r#"{"id":"note-id","title":"Title","content":"repeat\nrepeat"}"#,
                "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-repeat\n+new\n*** End Patch",
                "patch hunk context matched multiple locations",
            ),
        ];
        for (body, patch, expected) in cases {
            let server = SequenceServer::spawn([(200, body)]);
            let message = edit_note(&server.client(), input(patch))
                .await
                .expect_err("patch must conflict")
                .to_string();
            assert_eq!(message, expected);
            assert_eq!(server.finish().len(), 1);
        }
    }

    #[tokio::test]
    async fn team_edit_uses_team_item_route_and_team_patch_path() {
        let server = SequenceServer::spawn([
            (200, r#"{"id":"note-id","title":"Title","content":"old"}"#),
            (202, ""),
            (200, r#"{"id":"note-id","title":"Title","content":"new"}"#),
        ]);
        let patch = "*** Begin Patch\n*** Update File: teams/core/notes/note-id.md\n@@\n-old\n+new\n*** End Patch";
        let output = edit_note(
            &server.client(),
            EditNoteInput {
                workspace: Workspace::Team {
                    team_path: "core".to_owned(),
                },
                note_ref: "note-id".to_owned(),
                refresh: false,
                patch: patch.to_owned(),
            },
        )
        .await
        .expect("team edit should succeed")
        .expect("reference should resolve");
        assert_eq!(output.patch_path, "teams/core/notes/note-id.md");
        let requests = server.finish();
        assert!(requests[0].starts_with("GET /v1/teams/core/notes/note-id HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("PATCH /v1/teams/core/notes/note-id HTTP/1.1\r\n"));
        assert!(requests[2].starts_with("GET /v1/teams/core/notes/note-id HTTP/1.1\r\n"));
    }
}
