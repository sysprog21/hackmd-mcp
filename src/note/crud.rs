use std::collections::BTreeMap;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::{
        CommentPermission, CreateNoteRequest, NotePermission, NoteResponse, PatchField,
        PayloadError, SuggestEditPermission, UpdateNoteRequest, deserialize_patch_field,
    },
    models::Workspace,
    note::{
        get::{NoteDetail, normalize_note},
        reference::{NoteRefError, NoteResolution, ResolvedNoteRef},
    },
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateNoteInput {
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Optional note title; `HackMD` content front matter or a leading H1 may
    /// take precedence.
    pub(crate) title: Option<String>,
    /// Full Markdown note body.
    pub(crate) content: Option<String>,
    /// Complete replacement tag list.
    pub(crate) tags: Option<Vec<String>>,
    /// Optional note description.
    pub(crate) description: Option<String>,
    /// Custom URL slug.
    pub(crate) permalink: Option<String>,
    /// Who may read the note; omitted preserves the workspace default.
    pub(crate) read_permission: Option<NotePermission>,
    /// Who may edit the note; omitted preserves the workspace default.
    pub(crate) write_permission: Option<NotePermission>,
    /// Who may comment; omitted preserves the workspace default.
    pub(crate) comment_permission: Option<CommentPermission>,
    /// Who may suggest edits; omitted preserves the workspace default.
    pub(crate) suggest_edit_permission: Option<SuggestEditPermission>,
    /// Folder ID for placement. The server verifies POST placement and uses
    /// PATCH only as a compatibility fallback.
    pub(crate) parent_folder_id: Option<String>,
    /// Per-feature `HackMD` permission overrides.
    pub(crate) note_features: Option<BTreeMap<String, Value>>,
    /// Optional client origin identifier.
    pub(crate) origin: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateNoteInput {
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
    /// New title. `HackMD` may still derive the title from the body's first
    /// heading.
    pub(crate) title: Option<String>,
    /// Explicit full-body replacement. Prefer `hackmd_edit_note` for normal
    /// content edits.
    pub(crate) content: Option<String>,
    /// Complete replacement tag list.
    pub(crate) tags: Option<Vec<String>>,
    /// New description.
    pub(crate) description: Option<String>,
    /// New custom URL slug.
    pub(crate) permalink: Option<String>,
    /// Who may read the note.
    pub(crate) read_permission: Option<NotePermission>,
    /// Who may edit the note; may not exceed `read_permission`.
    pub(crate) write_permission: Option<NotePermission>,
    /// Folder ID, or null to move the note to the workspace root.
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    #[schemars(with = "Option<String>")]
    parent_folder_id: PatchField,
    /// Unsupported on PATCH; supplying this produces an explanatory tool error.
    pub(crate) comment_permission: Option<CommentPermission>,
    /// Unsupported on PATCH; supplying this produces an explanatory tool error.
    pub(crate) suggest_edit_permission: Option<SuggestEditPermission>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteNoteInput {
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
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateNoteOutput {
    /// The created note as read back, without its body.
    pub(crate) note: NoteDetail,
    pub(crate) folder_placement_requested: bool,
    /// Whether the note was actually read back inside the requested folder.
    /// False means `HackMD` accepted the placement but never showed it.
    pub(crate) folder_placement_confirmed: bool,
    pub(crate) compatibility_patch_applied: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct UpdateNoteOutput {
    /// The updated note as read back, without its body.
    pub(crate) note: NoteDetail,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeleteNoteOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
}

#[derive(Debug, Error)]
pub(crate) enum CrudError {
    #[error(
        "comment_permission and suggest_edit_permission are create-only; HackMD PATCH does not support changing them"
    )]
    UnsupportedPatchPermissions,
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    Api(#[from] HackmdError),
    #[error("HackMD accepted the update for note {note_id}, but read-back content did not match")]
    ReadbackMismatch { note_id: String },
    #[error("note {note_id} was created, but folder placement failed: {source}")]
    FolderPlacement {
        note_id: String,
        #[source]
        source: HackmdError,
    },
}

impl crate::reply::ToolError for CrudError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::UnsupportedPatchPermissions => ErrorKind::InvalidInput,
            Self::Payload(error) => error.kind(),
            Self::Reference(error) => error.kind(),
            Self::Api(error) => error.kind(),
            Self::ReadbackMismatch { .. } => ErrorKind::Readback,
            Self::FolderPlacement { .. } => ErrorKind::PartialWrite,
        }
    }
}

fn placed_in(note: &NoteResponse, folder_id: &str) -> bool {
    note.folder_paths.iter().any(|path| path.id == folder_id)
}

pub(crate) async fn create_note(
    client: &HackmdClient,
    input: CreateNoteInput,
) -> Result<CreateNoteOutput, CrudError> {
    let folder = input.parent_folder_id;
    let payload = CreateNoteRequest {
        title: input.title,
        content: input.content,
        tags: input.tags,
        description: input.description,
        read_permission: input.read_permission,
        write_permission: input.write_permission,
        comment_permission: input.comment_permission,
        suggest_edit_permission: input.suggest_edit_permission,
        permalink: input.permalink,
        parent_folder_id: folder.clone(),
        note_features: input.note_features,
        origin: input.origin,
    };
    payload.validate()?;
    client.ensure_team_exists(&input.workspace).await?;
    let mut note = client.create_note(&input.workspace, &payload).await?;
    let note_id = note.id.clone();
    let mut compatibility_patch_applied = false;
    if let Some(folder_id) = folder.as_ref() {
        let placement_failed = |source| CrudError::FolderPlacement {
            note_id: note_id.clone(),
            source,
        };

        // A single read, not a poll: POST placement is the common case, and
        // waiting out the read-back window here would tax every foldered create
        // to catch a case the PATCH below already repairs.
        note = client
            .get_note(&input.workspace, &note_id)
            .await
            .map_err(placement_failed)?;
        if !placed_in(&note, folder_id) {
            let placement = UpdateNoteRequest {
                parent_folder_id: Some(Some(folder_id.clone())),
                ..UpdateNoteRequest::default()
            };
            placement.validate()?;
            client
                .update_note(&input.workspace, &note_id, &placement)
                .await
                .map_err(placement_failed)?;
            compatibility_patch_applied = true;

            // Placement is a write like any other, so confirm it the way the
            // edit and push tools confirm theirs rather than trusting one
            // immediate read. Failing to confirm is reported as a flag, never
            // as an error: the note exists by now, and an agent that saw an
            // error would create a second one on retry.
            note = client
                .poll_readback(
                    0,
                    || client.get_note(&input.workspace, &note_id),
                    |note| placed_in(note, folder_id),
                )
                .await
                .map_err(placement_failed)?
                .value;
        }
    }

    // Judged from the note being returned, so the flag can never disagree with
    // the folder_ids the caller reads out of it.
    let folder_placement_confirmed = folder.as_ref().is_some_and(|id| placed_in(&note, id));
    Ok(CreateNoteOutput {
        note: without_content(
            ResolvedNoteRef {
                workspace: input.workspace,
                note_id,
            },
            note,
        ),
        folder_placement_requested: folder.is_some(),
        folder_placement_confirmed,
        compatibility_patch_applied,
    })
}

pub(crate) async fn update_note(
    client: &HackmdClient,
    input: UpdateNoteInput,
) -> Result<Result<UpdateNoteOutput, NoteResolution>, CrudError> {
    if input.comment_permission.is_some() || input.suggest_edit_permission.is_some() {
        return Err(CrudError::UnsupportedPatchPermissions);
    }
    let payload = UpdateNoteRequest {
        title: input.title,
        content: input.content,
        tags: input.tags,
        description: input.description,
        read_permission: input.read_permission,
        write_permission: input.write_permission,
        permalink: input.permalink,
        parent_folder_id: input.parent_folder_id.into_request(),
    };
    payload.validate()?;
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
    client
        .update_note(&note.workspace, &note.note_id, &payload)
        .await?;

    // Only a body replacement is compared. Metadata is not: `HackMD` derives a
    // title from the body's first heading, so a supplied title can legitimately
    // read back different, and waiting on it would only stall.
    let readback = client
        .poll_readback(
            payload.content.as_ref().map_or(0, String::len),
            || client.get_note(&note.workspace, &note.note_id),
            |readback| {
                payload
                    .content
                    .as_deref()
                    .is_none_or(|content| readback.content.as_deref() == Some(content))
            },
        )
        .await?;
    if !readback.confirmed {
        return Err(CrudError::ReadbackMismatch {
            note_id: note.note_id,
        });
    }
    Ok(Ok(UpdateNoteOutput {
        note: without_content(note, readback.value),
    }))
}

fn without_content(reference: ResolvedNoteRef, note: NoteResponse) -> NoteDetail {
    NoteDetail {
        content: None,
        ..normalize_note(reference, note)
    }
}

pub(crate) async fn delete_note(
    client: &HackmdClient,
    input: DeleteNoteInput,
) -> Result<Result<DeleteNoteOutput, NoteResolution>, CrudError> {
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
    client.delete_note(&note.workspace, &note.note_id).await?;
    Ok(Ok(DeleteNoteOutput {
        workspace: note.workspace,
        note_id: note.note_id,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CreateNoteInput, CrudError, UpdateNoteInput, create_note, update_note};
    use crate::{
        client::HackmdClient,
        config::Config,
        dto::PatchField,
        fixture::{Scenario, SequenceServer},
    };

    #[test]
    fn update_distinguishes_missing_folder_from_explicit_root() {
        let missing: UpdateNoteInput = serde_json::from_value(json!({"note_ref": "id"}))
            .expect("missing folder should deserialize");
        assert!(matches!(missing.parent_folder_id, PatchField::Unspecified));

        let root: UpdateNoteInput = serde_json::from_value(json!({
            "note_ref": "id",
            "parent_folder_id": null
        }))
        .expect("null folder should deserialize");
        assert!(matches!(root.parent_folder_id, PatchField::Set(None)));

        let folder: UpdateNoteInput = serde_json::from_value(json!({
            "note_ref": "id",
            "parent_folder_id": "folder-id"
        }))
        .expect("folder should deserialize");
        assert!(matches!(folder.parent_folder_id, PatchField::Set(Some(id)) if id == "folder-id"));
    }

    #[tokio::test]
    async fn unsupported_patch_permissions_fail_before_resolution_or_network() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let input: UpdateNoteInput = serde_json::from_value(json!({
            "note_ref": "id",
            "comment_permission": "everyone"
        }))
        .expect("unsupported permission remains structurally valid");
        assert!(matches!(
            update_note(&client, input).await,
            Err(CrudError::UnsupportedPatchPermissions)
        ));
    }

    #[tokio::test]
    async fn accepted_update_is_read_back() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("PATCH", "/v1/notes/note%2Fid", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/note%2Fid",
                200,
                r#"{"id":"note/id","title":"Updated"}"#,
            ),
        ]);
        let client = server.client();
        let input = serde_json::from_value(json!({
            "note_ref": "note/id",
            "title": "Updated"
        }))
        .expect("update input should deserialize");
        let output = update_note(&client, input)
            .await
            .expect("update should succeed")
            .expect("direct reference should resolve");
        assert_eq!(output.note.title, "Updated");
        assert_eq!(output.note.patch_path, "notes/note/id.md");
        server.finish();
    }

    #[tokio::test]
    async fn invalid_payloads_fail_before_resolution_or_network() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let empty_update: UpdateNoteInput =
            serde_json::from_value(json!({"note_ref": "id"})).expect("input should deserialize");
        assert!(matches!(
            update_note(&client, empty_update).await,
            Err(CrudError::Payload(crate::dto::PayloadError::EmptyPatch))
        ));

        let invalid_create: CreateNoteInput = serde_json::from_value(json!({
            "read_permission": "owner",
            "write_permission": "guest"
        }))
        .expect("input should deserialize");
        assert!(matches!(
            create_note(&client, invalid_create).await,
            Err(CrudError::Payload(
                crate::dto::PayloadError::WriteMorePermissiveThanRead
            ))
        ));
    }

    #[tokio::test]
    async fn unplaced_note_is_reported_not_failed() {
        const UNPLACED: &str = r#"{"id":"new-id","title":"New"}"#;

        // Create, first read, PATCH, then a read-back that never shows the
        // folder however many times it is retried.
        let server = SequenceServer::spawn_repeating([
            (201, UNPLACED),
            (200, UNPLACED),
            (202, ""),
            (200, UNPLACED),
        ]);
        let client = server.client();
        let input: CreateNoteInput = serde_json::from_value(json!({
            "title": "New",
            "parent_folder_id": "folder-id"
        }))
        .expect("create input should deserialize");

        let output = create_note(&client, input)
            .await
            .expect("an unconfirmed placement must not fail a note that exists");
        assert_eq!(output.note.id, "new-id");
        assert!(output.folder_placement_requested);
        assert!(!output.folder_placement_confirmed);
        assert!(output.compatibility_patch_applied);
        assert!(output.note.folder_ids.is_empty());
    }

    #[tokio::test]
    async fn create_folder_placement_falls_back_when_post_drops_it() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("POST", "/v1/notes", 201, r#"{"id":"new-id","title":"New"}"#)
                .expect_body("the note and requested folder", |body| {
                    body == r#"{"title":"New","parentFolderId":"folder-id"}"#
                }),
            Scenario::new(
                "GET",
                "/v1/notes/new-id",
                200,
                r#"{"id":"new-id","title":"New"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/new-id", 202, "")
                .expect_body("the compatibility folder placement", |body| {
                    body == r#"{"parentFolderId":"folder-id"}"#
                }),
            Scenario::new(
                "GET",
                "/v1/notes/new-id",
                200,
                r#"{"id":"new-id","title":"New","folderPaths":[{"id":"folder-id","name":"Folder"}]}"#,
            ),
        ]);
        let client = server.client();
        let input: CreateNoteInput = serde_json::from_value(json!({
            "title": "New",
            "parent_folder_id": "folder-id"
        }))
        .expect("create input should deserialize");

        let output = create_note(&client, input)
            .await
            .expect("create and placement should succeed");
        assert_eq!(output.note.id, "new-id");
        assert!(output.folder_placement_requested);
        assert!(output.folder_placement_confirmed);
        assert!(output.compatibility_patch_applied);
        server.finish();
        assert_eq!(output.note.folder_ids, ["folder-id"]);
    }

    #[tokio::test]
    async fn create_skips_compatibility_patch_when_post_assigns_folder() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("POST", "/v1/notes", 201, r#"{"id":"new-id","title":"New"}"#),
            Scenario::new(
                "GET",
                "/v1/notes/new-id",
                200,
                r#"{"id":"new-id","title":"New","folderPaths":[{"id":"folder-id","name":"Folder"}]}"#,
            ),
        ]);
        let client = server.client();
        let input: CreateNoteInput = serde_json::from_value(json!({
            "title": "New",
            "parent_folder_id": "folder-id"
        }))
        .expect("create input should deserialize");
        let output = create_note(&client, input)
            .await
            .expect("POST-assigned folder should succeed");
        assert!(!output.compatibility_patch_applied);
        server.finish();
    }

    #[tokio::test]
    async fn folder_placement_failure_reports_already_created_note_id() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new(
                "POST",
                "/v1/notes",
                201,
                r#"{"id":"recoverable-id","title":"New"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/recoverable-id",
                200,
                r#"{"id":"recoverable-id","title":"New"}"#,
            ),
            Scenario::new(
                "PATCH",
                "/v1/notes/recoverable-id",
                500,
                r#"{"error":"fixture"}"#,
            ),
        ]);
        let client = HackmdClient::new(Config::for_loopback_test_no_retry(
            &server.api_url,
            "fixture-token",
        ))
        .expect("fixture client should build");
        let input: CreateNoteInput = serde_json::from_value(json!({
            "parent_folder_id": "folder-id"
        }))
        .expect("create input should deserialize");
        let message = create_note(&client, input)
            .await
            .expect_err("placement failure should be reported")
            .to_string();
        server.finish();
        assert!(
            message.starts_with("note recoverable-id was created, but folder placement failed:")
        );
        assert!(message.contains("PATCH /v1/notes/recoverable-id"));
    }
}
