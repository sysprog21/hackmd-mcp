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
        validate_permission_order,
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
    /// The `HackMD` API cannot set this; supplying it is refused with an
    /// explanatory error. Change comment permission in the `HackMD` web UI.
    pub(crate) comment_permission: Option<CommentPermission>,
    /// The `HackMD` API cannot set this; supplying it is refused with an
    /// explanatory error. Change suggest-edit permission in the `HackMD` web
    /// UI.
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
    /// New title. A YAML `title:` in the body wins, then a leading H1, and
    /// only then this field, so on a note with either it changes nothing.
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
    /// The `HackMD` API cannot set this; supplying it is refused with an
    /// explanatory error. Change comment permission in the `HackMD` web UI.
    pub(crate) comment_permission: Option<CommentPermission>,
    /// The `HackMD` API cannot set this; supplying it is refused with an
    /// explanatory error. Change suggest-edit permission in the `HackMD` web
    /// UI.
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
    /// The `body_hash` from `hackmd_get_note` that the write was made against.
    /// The write is then refused if the body has changed since, so an edit
    /// made meanwhile, in the browser or by another agent, is not overwritten.
    /// A metadata-only change resends the current body, so it takes one too.
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
    Updated {
        note: Box<NoteDetail>,
        /// `null` when no folder was asked for. Otherwise whether that read
        /// shows the note in the requested folder (or at the root for
        /// `null`). Reported, not waited on: `HackMD` may show a move late,
        /// so `false` is not a failure, and since the read lists ancestors, a
        /// move up to an ancestor of the old folder shows `true` at once.
        folder_placement_confirmed: Option<bool>,
    },
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
        "comment_permission and suggest_edit_permission cannot be set through the HackMD API (neither on create nor on PATCH); change them in the HackMD web UI"
    )]
    UnsupportedPermissionField,
    #[error("patch cannot be combined with other fields; send it in a call of its own")]
    PatchWithFields,
    #[error("nothing to update: give a patch, content, or the metadata fields to change")]
    NothingToUpdate,
    #[error(
        "expected_hash must be a body_hash from hackmd_get_note: sha256: and 64 lowercase hex digits"
    )]
    MalformedExpectedHash,
    #[error(
        "restore takes the internal note ID from hackmd_list_notes with source trash; a trashed note has no live @owner/slug to resolve, and refresh does not apply"
    )]
    RestoreNeedsId,
    #[error(
        "cannot change note {note_id}'s metadata: HackMD returned no body to send back with it, and an update without one may blank the note"
    )]
    NoBodyToResend { note_id: String },
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
    #[error(
        "note {note_id} was created, but folder placement failed: {source}. Do not create it again; set its folder with hackmd_update_note parent_folder_id"
    )]
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
            Self::UnsupportedPermissionField
            | Self::PatchWithFields
            | Self::NothingToUpdate
            | Self::MalformedExpectedHash
            | Self::RestoreNeedsId
            | Self::TeamRestore => ErrorKind::InvalidInput,
            Self::BodyChanged(_) => ErrorKind::Conflict,
            Self::NoBodyToResend { .. } => ErrorKind::Upstream,
            Self::Edit(error) => error.kind(),
            Self::Payload(error) => error.kind(),
            Self::Reference(error) => error.kind(),
            Self::Api(error) => error.kind(),
            Self::FolderPlacement { .. } => ErrorKind::PartialWrite,
        }
    }
}

/// Whether a read shows the metadata a PATCH set, so a read from before the
/// PATCH is not reported as its result. Values are compared the way `HackMD`
/// may normalize them, so a write that landed is not failed over a format:
/// tags as a trimmed set without empties (as the official CLI sends them),
/// a missing description as empty, a permalink ignoring case. Left out: the
/// title, which `HackMD` may derive from the body, and the folder, whose read
/// is an ancestor list of unverified order (and unmeasured for teams), so a
/// move to an ancestor would match before it landed.
fn shows_metadata(sent: &UpdateNoteRequest<'_>, note: &NoteResponse) -> bool {
    fn tag_set(tags: &[String]) -> std::collections::BTreeSet<&str> {
        tags.iter()
            .map(|tag| tag.trim())
            .filter(|tag| !tag.is_empty())
            .collect()
    }
    sent.tags
        .as_ref()
        .is_none_or(|tags| tag_set(tags) == tag_set(&note.tags))
        && sent.description.as_ref().is_none_or(|description| {
            note.description.as_deref().unwrap_or_default() == description
        })
        && sent.permalink.as_ref().is_none_or(|permalink| {
            note.permalink
                .as_ref()
                .is_some_and(|read| read.eq_ignore_ascii_case(permalink))
        })
        && sent
            .read_permission
            .is_none_or(|permission| note.read_permission == Some(permission))
        && sent
            .write_permission
            .is_none_or(|permission| note.write_permission == Some(permission))
}

fn placed_in(note: &NoteResponse, folder_id: &str) -> bool {
    note.folder_paths.iter().any(|path| path.id == folder_id)
}

pub(crate) async fn create_note(
    client: &HackmdClient,
    input: CreateNoteInput,
) -> Result<CreateNoteOutput, CrudError> {
    // The HackMD API silently ignores these on create and rejects them on
    // PATCH, so refuse rather than appear to set something that never takes.
    if input.comment_permission.is_some() || input.suggest_edit_permission.is_some() {
        return Err(CrudError::UnsupportedPermissionField);
    }
    let folder = input.parent_folder_id;
    let mut payload = CreateNoteRequest {
        title: input.title,
        content: input.content,
        tags: input.tags,
        description: input.description,
        read_permission: input.read_permission,
        write_permission: input.write_permission,
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

        // The PATCH must carry the body (see `UpdateNoteRequest`). The one this
        // call just created the note with comes first: nobody else has edited a
        // note this new, while the read may be too early to show it. With
        // neither, there is no body to send, so placement is left unconfirmed
        // rather than risk blanking the note.
        let body = if placed_in(&note, folder_id) {
            None
        } else {
            payload.content.take().or_else(|| note.content.take())
        };
        if let Some(body) = body {
            let placement = UpdateNoteRequest {
                parent_folder_id: Some(Some(folder_id.clone())),
                ..UpdateNoteRequest::new(body)
            };
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
                    placement.content.len(),
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
    let reference = ResolvedNoteRef {
        workspace: input.workspace,
        note_id,
    };
    Ok(CreateNoteOutput {
        note: crate::local::offload(|| without_content(reference, note)),
        folder_placement_requested: folder.is_some(),
        folder_placement_confirmed,
        compatibility_patch_applied,
    })
}

pub(crate) async fn update_note(
    client: &HackmdClient,
    mut input: UpdateNoteInput,
) -> Result<Result<UpdateNoteOutput, NoteResolution>, CrudError> {
    if crate::hash::is_malformed(input.expected_hash.as_deref()) {
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
        return Err(CrudError::UnsupportedPermissionField);
    }
    validate_permission_order(input.read_permission, input.write_permission)?;
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

    // A metadata-only change resends the body it reads here. expected_hash is
    // checked against that same read, so what goes back is the body the agent
    // saw; an edit landing between this read and the PATCH is still reverted,
    // since `HackMD` has no conditional write.
    let replacing = input.content.is_some();
    let content = match (input.content, input.expected_hash.as_deref()) {
        (Some(content), None) => content,
        (replacement, expected) => {
            let resending = replacement.is_none();
            let (_, current) = client
                .get_note_body(&note.workspace, &note.note_id)
                .await
                .map_err(|error| match error {
                    HackmdError::MissingContent { note_id } if resending => {
                        CrudError::NoBodyToResend { note_id }
                    }
                    other => other.into(),
                })?;
            ensure_unchanged(&note.note_id, &current, expected)?;
            replacement.unwrap_or(current)
        }
    };
    let mut payload = UpdateNoteRequest {
        title: input.title,
        content: content.into(),
        tags: input.tags,
        description: input.description,
        read_permission: input.read_permission,
        write_permission: input.write_permission,
        permalink: input.permalink,
        parent_folder_id: input.parent_folder_id.into_request(),
    };
    client
        .update_note(&note.workspace, &note.note_id, &payload)
        .await?;

    // A body replacement is compared, and only it is kept alive for the
    // read-back; a resent body is not, since an edit can land after the PATCH.
    // The metadata sent is compared too (see `shows_metadata`).
    let written_bytes = payload.content.len();
    let body = std::mem::take(&mut payload.content);
    let expected = replacing.then_some(body);
    let written = client
        .confirm_note_write(&note.workspace, &note.note_id, written_bytes, |readback| {
            shows_metadata(&payload, readback)
                && expected
                    .as_deref()
                    .is_none_or(|body| readback.content.as_deref() == Some(body))
        })
        .await?;

    // Judged from the read being returned, like create's flag, so it can never
    // disagree with the folder_ids the caller reads out of it.
    let folder_placement_confirmed = payload
        .parent_folder_id
        .as_ref()
        .map(|folder| match folder {
            Some(id) => placed_in(&written, id),
            None => written.folder_paths.is_empty(),
        });
    Ok(Ok(UpdateNoteOutput::Updated {
        note: Box::new(crate::local::offload(|| without_content(note, written))),
        folder_placement_confirmed,
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
            "expected_hash": crate::hash::body_hash("as read")
        }))
        .expect("input should deserialize");
        assert!(matches!(
            update_note(&fixture.client(), input).await,
            Err(CrudError::BodyChanged(_))
        ));
        assert_eq!(fixture.finish().len(), 1, "nothing may be written");
    }

    #[tokio::test]
    async fn metadata_update_honors_expected_hash_on_the_body_it_resends() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/id",
            200,
            r#"{"id":"id","title":"T","content":"edited meanwhile"}"#,
        )]);
        let input = serde_json::from_value(json!({
            "note_ref": "id",
            "title": "New",
            "expected_hash": crate::hash::body_hash("as read")
        }))
        .expect("input should deserialize");
        assert!(matches!(
            update_note(&fixture.client(), input).await,
            Err(CrudError::BodyChanged(_))
        ));
        assert_eq!(
            fixture.finish().len(),
            1,
            "a stale body must not be sent back"
        );
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
    async fn unsupported_permission_fields_fail_before_resolution_or_network() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        // Refused on update (PATCH rejects them) ...
        let update: UpdateNoteInput = serde_json::from_value(json!({
            "note_ref": "id",
            "comment_permission": "everyone"
        }))
        .expect("unsupported permission remains structurally valid");
        assert!(matches!(
            update_note(&client, update).await,
            Err(CrudError::UnsupportedPermissionField)
        ));
        // ... and on create (the API silently drops them), so neither pretends.
        let create: CreateNoteInput = serde_json::from_value(json!({
            "suggest_edit_permission": "owners"
        }))
        .expect("unsupported permission remains structurally valid");
        assert!(matches!(
            create_note(&client, create).await,
            Err(CrudError::UnsupportedPermissionField)
        ));
    }

    #[tokio::test]
    async fn metadata_only_update_resends_the_body_so_hackmd_cannot_blank_it() {
        // HackMD clears the body on a PATCH that omits content, so a metadata
        // change (here a title) must first read the body and send it back. The
        // read-back shows an edit typed after the PATCH; a resent body is not
        // compared, so that must not fail a title change that took.
        let server = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note%2Fid",
                200,
                r#"{"id":"note/id","title":"Old","content":"keep me"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note%2Fid", 202, "").expect_body(
                "the metadata change plus the preserved body",
                |body| {
                    body.contains(r#""title":"Updated""#) && body.contains(r#""content":"keep me""#)
                },
            ),
            Scenario::new(
                "GET",
                "/v1/notes/note%2Fid",
                200,
                r#"{"id":"note/id","title":"Updated","content":"typed after"}"#,
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
        let super::UpdateNoteOutput::Updated { note, .. } = output else {
            panic!("fields without a patch should update the note");
        };
        assert_eq!(note.title, "Updated");
        assert_eq!(note.patch_path, "notes/note/id.md");
        server.finish();
    }

    #[tokio::test]
    async fn a_metadata_update_without_a_body_to_resend_says_why() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/id",
            200,
            r#"{"id":"id","title":"T"}"#,
        )]);
        let input = serde_json::from_value(json!({"note_ref": "id", "title": "New"}))
            .expect("input should deserialize");
        let error = update_note(&fixture.client(), input)
            .await
            .expect_err("nothing can be sent without a body");
        assert!(matches!(error, CrudError::NoBodyToResend { .. }), "{error}");
        assert_eq!(fixture.finish().len(), 1, "nothing may be written");
    }

    #[test]
    fn metadata_is_compared_the_way_hackmd_may_normalize_it() {
        use super::{NoteResponse, UpdateNoteRequest, shows_metadata};
        use crate::dto::NotePermission;

        let read = |value: serde_json::Value| -> NoteResponse {
            serde_json::from_value(value).expect("note should deserialize")
        };
        let sent = UpdateNoteRequest {
            tags: Some(vec![
                " a".to_owned(),
                "b".to_owned(),
                "b".to_owned(),
                String::new(),
            ]),
            description: Some(String::new()),
            permalink: Some("My-Note".to_owned()),
            read_permission: Some(NotePermission::Guest),
            write_permission: Some(NotePermission::Owner),
            parent_folder_id: Some(Some("elsewhere".to_owned())),
            ..UpdateNoteRequest::new("body")
        };
        let shown = read(json!({
            "id": "n", "title": "T", "tags": ["b", "a"], "permalink": "my-note",
            "readPermission": "guest", "writePermission": "owner"
        }));
        assert!(
            shows_metadata(&sent, &shown),
            "normalized values and an unconfirmed folder must match"
        );
        for stale in [
            json!({"id": "n", "title": "T", "tags": ["a"], "permalink": "my-note",
                   "readPermission": "guest", "writePermission": "owner"}),
            json!({"id": "n", "title": "T", "tags": ["a", "b"], "description": "old",
                   "permalink": "my-note", "readPermission": "guest", "writePermission": "owner"}),
            json!({"id": "n", "title": "T", "tags": ["a", "b"], "permalink": "other",
                   "readPermission": "guest", "writePermission": "owner"}),
            json!({"id": "n", "title": "T", "tags": ["a", "b"], "permalink": "my-note",
                   "readPermission": "owner", "writePermission": "owner"}),
            json!({"id": "n", "title": "T", "tags": ["a", "b"], "permalink": "my-note",
                   "readPermission": "guest", "writePermission": "signed_in"}),
        ] {
            assert!(!shows_metadata(&sent, &read(stale.clone())), "{stale}");
        }
    }

    #[tokio::test]
    async fn metadata_that_never_shows_is_a_readback_error() {
        let fixture = SequenceServer::spawn_repeating([
            (200, r#"{"id":"id","title":"T","content":"body"}"#),
            (202, ""),
            (
                200,
                r#"{"id":"id","title":"T","content":"body","tags":["old"]}"#,
            ),
        ]);
        let input = serde_json::from_value(json!({"note_ref": "id", "tags": ["new"]}))
            .expect("input should deserialize");
        assert!(matches!(
            update_note(&fixture.client(), input).await,
            Err(CrudError::Api(super::HackmdError::ReadbackMismatch { .. }))
        ));
    }

    #[tokio::test]
    async fn a_folder_move_the_read_does_not_show_is_reported_unconfirmed() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"T","content":"body","folderPaths":[{"id":"old","name":"Old"}]}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"T","content":"body","folderPaths":[{"id":"old","name":"Old"}]}"#,
            ),
        ]);
        let input = serde_json::from_value(json!({"note_ref": "id", "parent_folder_id": "new"}))
            .expect("input should deserialize");
        let output = update_note(&server.client(), input)
            .await
            .expect("update should succeed")
            .expect("direct reference should resolve");
        server.finish();
        let super::UpdateNoteOutput::Updated {
            note,
            folder_placement_confirmed,
        } = output
        else {
            panic!("fields without a patch should update the note");
        };
        assert_eq!(folder_placement_confirmed, Some(false));
        assert_eq!(note.folder_ids, ["old"]);
    }

    #[tokio::test]
    async fn a_metadata_update_waits_for_a_read_that_shows_it() {
        // The first read-back predates the PATCH and must not be reported as
        // its result; the second shows the tags, in another order.
        let server = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"T","content":"body"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"T","content":"body","tags":[]}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/id",
                200,
                r#"{"id":"id","title":"T","content":"body","tags":["b","a"]}"#,
            ),
        ]);
        let input = serde_json::from_value(json!({"note_ref": "id", "tags": ["a", "b"]}))
            .expect("input should deserialize");
        let output = update_note(&server.client(), input)
            .await
            .expect("update should succeed")
            .expect("direct reference should resolve");
        let super::UpdateNoteOutput::Updated { note, .. } = output else {
            panic!("fields without a patch should update the note");
        };
        assert_eq!(note.tags, ["b", "a"]);
        server.finish();
    }

    /// A body that never shows up is an error naming the recovery, not a
    /// success, and not a prompt to send the body again blind.
    #[tokio::test]
    async fn a_content_write_that_never_shows_up_names_the_recovery() {
        let fixture = SequenceServer::spawn_repeating([
            (202, ""),
            (200, r#"{"id":"note-id","title":"T","content":"old"}"#),
        ]);
        let input = serde_json::from_value(json!({
            "note_ref": "note-id",
            "content": "new"
        }))
        .expect("update input should deserialize");
        let error = update_note(&fixture.client(), input)
            .await
            .expect_err("an unconfirmed write should fail");
        assert_eq!(
            error.to_string(),
            "HackMD accepted the update for note note-id, but no read-back showed it; call hackmd_get_note and compare before writing again"
        );
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
        const UNPLACED: &str = r#"{"id":"new-id","title":"New","content":"kept"}"#;

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
                r#"{"id":"new-id","title":"New","content":"kept"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/new-id", 202, "")
                .expect_body("the folder placement carrying the body back", |body| {
                    body == r#"{"content":"kept","parentFolderId":"folder-id"}"#
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
            // The read does not show the body yet; the PATCH must still carry
            // the one this call created the note with, never a blank.
            Scenario::new(
                "PATCH",
                "/v1/notes/recoverable-id",
                500,
                r#"{"error":"fixture"}"#,
            )
            .expect_body("the created body", |body| {
                body == r#"{"content":"created body","parentFolderId":"folder-id"}"#
            }),
        ]);
        let client = HackmdClient::new(Config::for_loopback_test_no_retry(
            &server.api_url,
            "fixture-token",
        ))
        .expect("fixture client should build");
        let input: CreateNoteInput = serde_json::from_value(json!({
            "content": "created body",
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
        assert!(message.contains("Do not create it again"), "{message}");
    }

    #[tokio::test]
    async fn an_unreadable_create_reply_says_look_not_retry() {
        let server =
            SequenceServer::spawn_scenarios([Scenario::new("POST", "/v1/notes", 201, "not json")]);
        let input: CreateNoteInput =
            serde_json::from_value(json!({"title": "New"})).expect("input should deserialize");
        let error = create_note(&server.client(), input)
            .await
            .expect_err("an unreadable reply should fail");
        server.finish();
        assert!(
            matches!(
                error,
                CrudError::Api(super::HackmdError::WriteUnconfirmed { .. })
            ),
            "{error}"
        );
    }

    #[tokio::test]
    async fn placement_sends_the_created_body_over_a_stale_blank_read() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("POST", "/v1/notes", 201, r#"{"id":"new-id","title":"New"}"#),
            // Too early to show the body just created.
            Scenario::new(
                "GET",
                "/v1/notes/new-id",
                200,
                r#"{"id":"new-id","title":"New","content":""}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/new-id", 202, "")
                .expect_body("the created body, not the stale blank", |body| {
                    body == r#"{"content":"created","parentFolderId":"folder-id"}"#
                }),
            Scenario::new(
                "GET",
                "/v1/notes/new-id",
                200,
                r#"{"id":"new-id","title":"New","content":"created","folderPaths":[{"id":"folder-id","name":"F"}]}"#,
            ),
        ]);
        let input: CreateNoteInput = serde_json::from_value(json!({
            "content": "created",
            "parent_folder_id": "folder-id"
        }))
        .expect("create input should deserialize");
        let output = create_note(&server.client(), input)
            .await
            .expect("create and placement should succeed");
        server.finish();
        assert!(output.folder_placement_confirmed);
    }

    #[tokio::test]
    async fn placement_without_any_body_is_left_unconfirmed_not_blanked() {
        let server = SequenceServer::spawn_scenarios([
            Scenario::new("POST", "/v1/notes", 201, r#"{"id":"new-id","title":"New"}"#),
            Scenario::new(
                "GET",
                "/v1/notes/new-id",
                200,
                r#"{"id":"new-id","title":"New"}"#,
            ),
        ]);
        let input: CreateNoteInput = serde_json::from_value(json!({
            "title": "New",
            "parent_folder_id": "folder-id"
        }))
        .expect("create input should deserialize");
        let output = create_note(&server.client(), input)
            .await
            .expect("an unplaced note is reported, not failed");
        server.finish();
        assert!(!output.folder_placement_confirmed);
        assert!(!output.compatibility_patch_applied);
    }
}
