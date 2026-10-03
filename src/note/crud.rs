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
        edit::{
            BodyChanged, EditNoteError, EditNoteInput, EditNoteOutput, edit_note, ensure_unchanged,
        },
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
    /// Explicit full-body replacement. Prefer `patch` for normal content
    /// edits: this overwrites the complete unversioned body.
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
    /// The default way to edit the body: one patch applied to the current
    /// body only when every hunk's context matches exactly once, then
    /// confirmed by reading it back. It cannot be combined with other fields.
    /// Format:
    /// `*** Begin Patch`, `*** Update File: <patch_path from hackmd_get_note>`,
    /// then hunks each opened by `@@` whose lines start with ` ` (context),
    /// `-` (remove), or `+` (add), then `*** End Patch`. Text after `@@` is an
    /// anchor that must equal exactly one line, ignoring surrounding
    /// whitespace; the hunk then applies after it, and an addition-only hunk
    /// goes directly below it. A line range such as `@@ -3,4 +3,5 @@` is not
    /// an anchor. A hunk closed by `*** End of File` must match the end of
    /// the body, and an addition-only one appends there.
    pub(crate) patch: Option<String>,
    /// The `body_hash` from `hackmd_get_note` that a `patch` or `content` was
    /// written against. The write is then refused if the body has changed
    /// since, so an edit made meanwhile, in the browser or by another agent,
    /// is not overwritten.
    pub(crate) expected_hash: Option<String>,
}

impl UpdateNoteInput {
    /// Whether anything besides `patch` would be changed.
    fn changes_fields(&self) -> bool {
        self.title.is_some()
            || self.content.is_some()
            || self.tags.is_some()
            || self.description.is_some()
            || self.permalink.is_some()
            || self.read_permission.is_some()
            || self.write_permission.is_some()
            || self.parent_folder_id.is_specified()
            || self.comment_permission.is_some()
            || self.suggest_edit_permission.is_some()
    }
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
    /// With `restore`, the internal ID from `hackmd_list_notes` with source
    /// `trash`.
    #[serde(alias = "note_id")]
    pub(crate) note_ref: String,
    /// Bypass the 60-second account and note-list caches when resolving an
    /// `@owner/slug` URL.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// Bring a personal note back from trash instead of deleting it.
    #[serde(default)]
    pub(crate) restore: bool,
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

/// Tagged with `mode` (`updated` or `patched`) so a caller can tell the two
/// shapes apart without guessing.
#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum UpdateNoteOutput {
    /// Fields were set: the note as read back, without its body.
    Updated { note: Box<NoteDetail> },
    /// A patch was applied, or matched a body that already had it.
    Patched(EditNoteOutput),
}

#[derive(Debug, Serialize)]
pub(crate) struct DeleteNoteOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    /// True when the note came back from trash rather than going into it.
    pub(crate) restored: bool,
}

#[derive(Debug, Error)]
pub(crate) enum CrudError {
    #[error(
        "comment_permission and suggest_edit_permission are create-only; HackMD PATCH does not support changing them"
    )]
    UnsupportedPatchPermissions,
    #[error("patch cannot be combined with other fields; send it in a call of its own")]
    PatchWithFields,
    #[error("nothing to update: give a patch, content, or the metadata fields to change")]
    NothingToUpdate,
    #[error("expected_hash guards a body write; give it with patch or content")]
    ExpectedHashWithoutBody,
    #[error(
        "expected_hash must be a body_hash from hackmd_get_note: sha256: and 64 lowercase hex digits"
    )]
    MalformedExpectedHash,
    #[error(
        "restore takes the internal note ID from hackmd_list_notes with source trash; a trashed note has no live @owner/slug to resolve, and refresh does not apply"
    )]
    RestoreNeedsId,
    #[error(transparent)]
    BodyChanged(#[from] BodyChanged),
    #[error("only personal notes can be restored from trash; team deletion has no restore")]
    TeamRestore,
    #[error(transparent)]
    Edit(#[from] EditNoteError),
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    Api(#[from] HackmdError),
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
            Self::UnsupportedPatchPermissions
            | Self::PatchWithFields
            | Self::NothingToUpdate
            | Self::ExpectedHashWithoutBody
            | Self::MalformedExpectedHash
            | Self::RestoreNeedsId
            | Self::TeamRestore => ErrorKind::InvalidInput,
            Self::BodyChanged(_) => ErrorKind::Conflict,
            Self::Edit(error) => error.kind(),
            Self::Payload(error) => error.kind(),
            Self::Reference(error) => error.kind(),
            Self::Api(error) => error.kind(),
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
    mut input: UpdateNoteInput,
) -> Result<Result<UpdateNoteOutput, NoteResolution>, CrudError> {
    if input
        .expected_hash
        .as_deref()
        .is_some_and(|hash| !crate::sync::state::is_body_hash(hash))
    {
        return Err(CrudError::MalformedExpectedHash);
    }
    if let Some(patch) = input.patch.take() {
        if input.changes_fields() {
            return Err(CrudError::PatchWithFields);
        }
        let edit = EditNoteInput {
            workspace: input.workspace,
            note_ref: input.note_ref,
            refresh: input.refresh,
            patch,
            expected_hash: input.expected_hash,
        };
        return Ok(edit_note(client, edit)
            .await?
            .map(UpdateNoteOutput::Patched));
    }
    if !input.changes_fields() {
        return Err(CrudError::NothingToUpdate);
    }
    if input.comment_permission.is_some() || input.suggest_edit_permission.is_some() {
        return Err(CrudError::UnsupportedPatchPermissions);
    }
    if input.expected_hash.is_some() && input.content.is_none() {
        return Err(CrudError::ExpectedHashWithoutBody);
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
    if let Some(expected) = input.expected_hash.as_deref() {
        let (_, current) = client.get_note_body(&note.workspace, &note.note_id).await?;
        ensure_unchanged(&note.note_id, &current, Some(expected))?;
    }
    client
        .update_note(&note.workspace, &note.note_id, &payload)
        .await?;

    // Only a body replacement is compared. Metadata is not: `HackMD` derives a
    // title from the body's first heading, so a supplied title can legitimately
    // read back different, and waiting on it would only stall.
    let written = client
        .confirm_note_write(
            &note.workspace,
            &note.note_id,
            payload.content.as_ref().map_or(0, String::len),
            |readback| {
                payload
                    .content
                    .as_deref()
                    .is_none_or(|content| readback.content.as_deref() == Some(content))
            },
        )
        .await?;
    Ok(Ok(UpdateNoteOutput::Updated {
        note: Box::new(crate::local::offload(|| without_content(note, written))),
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
    // Resolving a slug searches the live workspace list, where a trashed note
    // never appears, so it could only end in a confusing not-found.
    if input.restore && (input.refresh || crate::note::reference::is_slug_url(&input.note_ref)) {
        return Err(CrudError::RestoreNeedsId);
    }
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
    if input.restore {
        // HackMD's trash is personal only: a deleted team note is gone.
        if !matches!(note.workspace, Workspace::Personal) {
            return Err(CrudError::TeamRestore);
        }
        client.restore_note(&note.note_id).await?;
    } else {
        client.delete_note(&note.workspace, &note.note_id).await?;
    }
    Ok(Ok(DeleteNoteOutput {
        workspace: note.workspace,
        note_id: note.note_id,
        restored: input.restore,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CreateNoteInput, CrudError, UpdateNoteInput, create_note, delete_note, update_note,
    };
    use crate::note::reference::NoteRefError;
    use crate::{
        client::HackmdClient,
        config::Config,
        dto::PatchField,
        fixture::{Scenario, SequenceServer},
    };

    #[tokio::test]
    async fn restore_encodes_id_and_accepts_empty_or_json_responses() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("PUT", "/v1/trash/folder%2Fid/restore", 202, ""),
            Scenario::new(
                "PUT",
                "/v1/trash/other/restore",
                200,
                r#"{"restored":true}"#,
            ),
        ]);
        let client = fixture.client();
        for note_id in ["folder/id", "other"] {
            let input = serde_json::from_value(json!({"note_ref": note_id, "restore": true}))
                .expect("restore input should deserialize");
            let restored = delete_note(&client, input)
                .await
                .expect("restore should succeed")
                .expect("a direct ID resolves without a request");
            assert_eq!(restored.note_id, note_id);
            assert!(restored.restored);
        }
        fixture.finish();
    }

    #[tokio::test]
    async fn update_and_restore_refuse_inputs_that_cannot_work_before_any_request() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let update = |value| {
            serde_json::from_value::<UpdateNoteInput>(value).expect("input should deserialize")
        };
        assert!(matches!(
            update_note(&client, update(json!({"note_ref": "id"}))).await,
            Err(CrudError::NothingToUpdate)
        ));
        assert!(matches!(
            update_note(
                &client,
                update(json!({
                    "note_ref": "id",
                    "title": "T",
                    "expected_hash": crate::sync::state::body_hash("")
                }))
            )
            .await,
            Err(CrudError::ExpectedHashWithoutBody)
        ));
        for malformed in ["sha256:x", "SHA256:00", " sha256:00"] {
            assert!(matches!(
                update_note(
                    &client,
                    update(json!({"note_ref": "id", "content": "c", "expected_hash": malformed}))
                )
                .await,
                Err(CrudError::MalformedExpectedHash)
            ));
        }
        for input in [
            json!({"note_ref": "https://hackmd.io/@alice/slug", "restore": true}),
            json!({"note_ref": "id", "restore": true, "refresh": true}),
        ] {
            let input = serde_json::from_value(input).expect("input should deserialize");
            assert!(matches!(
                delete_note(&client, input).await,
                Err(CrudError::RestoreNeedsId)
            ));
        }
        // The old restore tool took `note_id`; it still reaches `note_ref`.
        let input = serde_json::from_value(json!({"note_id": " ", "restore": true}))
            .expect("the older note_id spelling should deserialize");
        assert!(matches!(
            delete_note(&client, input).await,
            Err(CrudError::Reference(NoteRefError::Empty))
        ));
    }

    #[tokio::test]
    async fn content_replacement_honors_expected_hash() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/id",
            200,
            r#"{"id":"id","title":"T","content":"edited meanwhile"}"#,
        )]);
        let input = serde_json::from_value(json!({
            "note_ref": "id",
            "content": "replacement",
            "expected_hash": crate::sync::state::body_hash("as read")
        }))
        .expect("input should deserialize");
        assert!(matches!(
            update_note(&fixture.client(), input).await,
            Err(CrudError::BodyChanged(_))
        ));
        assert_eq!(fixture.finish().len(), 1, "nothing may be written");
    }

    #[tokio::test]
    async fn team_notes_cannot_be_restored_and_patch_stands_alone() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let input = serde_json::from_value(json!({
            "note_ref": "id", "team_path": "core", "restore": true
        }))
        .expect("restore input should deserialize");
        assert!(matches!(
            delete_note(&client, input).await,
            Err(CrudError::TeamRestore)
        ));
        let input = serde_json::from_value(json!({
            "note_ref": "id", "title": "T", "patch": "*** Begin Patch"
        }))
        .expect("update input should deserialize");
        assert!(matches!(
            update_note(&client, input).await,
            Err(CrudError::PatchWithFields)
        ));
    }

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
        let super::UpdateNoteOutput::Updated { note } = output else {
            panic!("fields without a patch should update the note");
        };
        assert_eq!(note.title, "Updated");
        assert_eq!(note.patch_path, "notes/note/id.md");
        server.finish();
    }

    #[tokio::test]
    async fn invalid_payloads_fail_before_resolution_or_network() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let empty_update: UpdateNoteInput =
            serde_json::from_value(json!({"note_ref": "id"})).expect("input should deserialize");
        assert!(matches!(
            update_note(&client, empty_update).await,
            Err(CrudError::NothingToUpdate)
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
        assert_eq!(output.note.folder_ids, Vec::<String>::new());
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
