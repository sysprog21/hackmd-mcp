use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::{
        CommentPermission, NotePermission, NotePublishType, NoteResponse, SimpleUserProfileResponse,
    },
    models::Workspace,
    note_ref::{NoteRefError, NoteResolution, ResolvedNoteRef},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetNoteInput {
    /// Workspace used for a direct internal note ID; scoped URLs resolve their own workspace.
    #[serde(default)]
    pub(crate) workspace: Workspace,
    /// Internal API ID, `hackmd.io/<id>`, or `hackmd.io/@owner/slug` URL.
    pub(crate) note_ref: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NoteDetail {
    pub(crate) id: String,
    pub(crate) short_id: Option<String>,
    pub(crate) title: String,
    pub(crate) content: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) workspace: Workspace,
    pub(crate) patch_path: String,
    pub(crate) folder_ids: Vec<String>,
    pub(crate) created_at: Option<i64>,
    pub(crate) last_changed_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title_updated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tags_updated_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_change_user: Option<SimpleUserProfileResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) publish_type: Option<NotePublishType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) published_at: Option<i64>,
    pub(crate) publish_link: Option<String>,
    pub(crate) permalink: Option<String>,
    pub(crate) user_path: Option<String>,
    pub(crate) team_path: Option<String>,
    pub(crate) read_permission: Option<NotePermission>,
    pub(crate) write_permission: Option<NotePermission>,
    pub(crate) comment_permission: Option<CommentPermission>,
}

#[derive(Debug, Error)]
pub(crate) enum GetNoteError {
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn get_note(
    client: &HackmdClient,
    input: GetNoteInput,
) -> Result<Result<NoteDetail, NoteResolution>, GetNoteError> {
    let resolution =
        crate::note_ref::resolve_note_ref(client, input.workspace, &input.note_ref).await?;
    let NoteResolution::Resolved { note } = resolution else {
        return Ok(Err(resolution));
    };
    let response = client.get_note(&note.workspace, &note.note_id).await?;
    Ok(Ok(normalize_note(note, response)))
}

fn normalize_note(reference: ResolvedNoteRef, note: NoteResponse) -> NoteDetail {
    let patch_path = patch_path(&reference.workspace, &reference.note_id);
    NoteDetail {
        id: note.id,
        short_id: note.short_id,
        title: note.title,
        content: note.content,
        description: note.description,
        tags: note.tags,
        workspace: reference.workspace,
        patch_path,
        folder_ids: note
            .folder_paths
            .into_iter()
            .map(|folder| folder.id)
            .collect(),
        created_at: note.created_at,
        last_changed_at: note.last_changed_at,
        title_updated_at: note.title_updated_at,
        tags_updated_at: note.tags_updated_at,
        last_change_user: note.last_change_user,
        publish_type: note.publish_type,
        published_at: note.published_at,
        publish_link: note.publish_link,
        permalink: note.permalink,
        user_path: note.user_path,
        team_path: note.team_path,
        read_permission: note.read_permission,
        write_permission: note.write_permission,
        comment_permission: note.comment_permission,
    }
}

pub(crate) fn patch_path(workspace: &Workspace, note_id: &str) -> String {
    match workspace {
        Workspace::Personal => format!("notes/{note_id}.md"),
        Workspace::Team { team_path } => format!("teams/{team_path}/notes/{note_id}.md"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{GetNoteInput, normalize_note};
    use crate::{
        client::HackmdClient, config::Config, dto::NoteResponse, models::Workspace,
        note_ref::ResolvedNoteRef,
    };

    fn response() -> NoteResponse {
        serde_json::from_value(json!({
            "id": "internal-id",
            "shortId": "short",
            "title": "Title",
            "content": "# Body",
            "titleUpdatedAt": 10.4,
            "tagsUpdatedAt": 11,
            "publishType": "view",
            "publishedAt": 12.6,
            "folderPaths": [
                {"id": "parent", "name": "Parent", "parentId": null, "icon": null},
                {"id": "child", "name": "Child", "parentId": "parent", "icon": null}
            ]
        }))
        .expect("detail fixture should deserialize")
    }

    #[test]
    fn personal_detail_has_exact_patch_path_and_flat_folder_ids() {
        let detail = normalize_note(
            ResolvedNoteRef {
                workspace: Workspace::Personal,
                note_id: "internal-id".to_owned(),
            },
            response(),
        );
        assert_eq!(detail.patch_path, "notes/internal-id.md");
        assert_eq!(detail.folder_ids, ["parent", "child"]);
        let value = serde_json::to_value(detail).expect("detail should serialize");
        assert_eq!(value["content"], "# Body");
        assert_eq!(value["titleUpdatedAt"], 10);
        assert_eq!(value["tagsUpdatedAt"], 11);
        assert_eq!(value["publishType"], "view");
        assert_eq!(value["publishedAt"], 13);
        assert!(value.get("folderPaths").is_none());
    }

    #[test]
    fn team_patch_path_is_deliberately_unencoded() {
        let detail = normalize_note(
            ResolvedNoteRef {
                workspace: Workspace::Team {
                    team_path: "team/path".to_owned(),
                },
                note_id: "note/id".to_owned(),
            },
            response(),
        );
        assert_eq!(detail.patch_path, "teams/team/path/notes/note/id.md");
    }

    #[tokio::test]
    async fn direct_reference_fetches_item_without_listing() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        let error = super::get_note(
            &client,
            GetNoteInput {
                workspace: Workspace::Personal,
                note_ref: "internal-id".to_owned(),
            },
        )
        .await
        .expect_err("missing token should fail at the item request");
        assert_eq!(
            error.to_string(),
            "GET /v1/notes/internal-id: HACKMD_API_TOKEN is not configured; set it in the server environment and restart the MCP server"
        );
    }
}
