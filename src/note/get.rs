use std::path::PathBuf;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::{
        CommentPermission, NotePermission, NotePublishType, NoteResponse, SimpleUserProfileResponse,
    },
    local::LocalFiles,
    models::Workspace,
    note::reference::{NoteRefError, NoteResolution, ResolvedNoteRef},
    sync::check::{CheckNoteSyncError, CheckNoteSyncOutput, check_note_sync},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetNoteInput {
    /// Team path (from `hackmd_get_me`) when a direct internal note ID belongs
    /// to a team; omit for personal notes. `@owner/slug` URLs name their own
    /// workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Internal API ID, `hackmd.io/<id>`, or `hackmd.io/@owner/slug` URL.
    /// Give this or `local_path`, not both.
    pub(crate) note_ref: Option<String>,
    /// Bypass the 60-second account and note-list caches when resolving an
    /// `@owner/slug` URL.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// Instead of `note_ref`: the absolute path of a Markdown file tracked by
    /// `hackmd_pull_note`. The result is then that file's sync state, compared
    /// three ways against its baseline and the remote note, without writing:
    /// `in_sync`, `local_changed`, `remote_changed`, or `conflict`, with the
    /// SHA-256 of each body.
    pub(crate) local_path: Option<PathBuf>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct NoteDetail {
    pub(crate) id: String,
    pub(crate) short_id: Option<String>,
    pub(crate) title: String,
    /// The Markdown body. Write tools leave it out: the caller just sent it,
    /// and echoing a large note back only spends context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<String>,
    /// SHA-256 of the body as read. Pass it back as `expected_hash` on
    /// `hackmd_update_note` to write only if nobody changed the body since.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) body_hash: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
    #[serde(rename = "team_path")]
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
    pub(crate) read_permission: Option<NotePermission>,
    pub(crate) write_permission: Option<NotePermission>,
    pub(crate) comment_permission: Option<CommentPermission>,
}

#[derive(Debug, Error)]
pub(crate) enum GetNoteError {
    #[error("give either note_ref or local_path")]
    NoteRefOrLocalPath,
    #[error("local_path is looked up in the local sync records; omit team_path and refresh")]
    SyncFilters,
    #[error(transparent)]
    Reference(#[from] NoteRefError),
    #[error(transparent)]
    Sync(#[from] CheckNoteSyncError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

impl crate::reply::ToolError for GetNoteError {
    fn kind(&self) -> crate::reply::ErrorKind {
        match self {
            Self::NoteRefOrLocalPath | Self::SyncFilters => crate::reply::ErrorKind::InvalidInput,
            Self::Reference(error) => error.kind(),
            Self::Sync(error) => error.kind(),
            Self::Api(error) => error.kind(),
        }
    }
}

/// One note, or the sync state of a tracked file.
/// Tagged with `mode` (`note` or `sync`) so a caller can tell the two shapes
/// apart without guessing.
#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(crate) enum GetNoteOutput {
    Note(Box<NoteDetail>),
    Sync(CheckNoteSyncOutput),
}

pub(crate) async fn get_note(
    client: &HackmdClient,
    files: &LocalFiles,
    input: GetNoteInput,
) -> Result<Result<GetNoteOutput, NoteResolution>, GetNoteError> {
    let note_ref = match (input.note_ref, input.local_path) {
        (Some(note_ref), None) => note_ref,
        (None, Some(local_path)) => {
            // The record names the note, so a team or a cache bypass here would
            // only look like it mattered.
            if input.workspace != Workspace::Personal || input.refresh {
                return Err(GetNoteError::SyncFilters);
            }
            let sync = check_note_sync(client, files, &local_path).await?;
            return Ok(Ok(GetNoteOutput::Sync(sync)));
        }
        _ => return Err(GetNoteError::NoteRefOrLocalPath),
    };
    let resolution =
        crate::note::reference::resolve_note_ref(client, input.workspace, &note_ref, input.refresh)
            .await?;
    let NoteResolution::Resolved { note } = resolution else {
        return Ok(Err(resolution));
    };
    let response = client.get_note(&note.workspace, &note.note_id).await?;
    let detail = crate::local::offload(|| normalize_note(note, response));
    Ok(Ok(GetNoteOutput::Note(Box::new(detail))))
}

pub(crate) fn normalize_note(reference: ResolvedNoteRef, note: NoteResponse) -> NoteDetail {
    let patch_path = crate::note::patch::patch_path(&reference.workspace, &reference.note_id);
    NoteDetail {
        id: note.id,
        short_id: note.short_id,
        title: note.title,
        body_hash: note.content.as_deref().map(crate::hash::body_hash),
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
        read_permission: note.read_permission,
        write_permission: note.write_permission,
        comment_permission: note.comment_permission,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{GetNoteInput, normalize_note};
    use crate::{
        dto::NoteResponse,
        fixture::{Scenario, SequenceServer},
        models::Workspace,
        note::reference::ResolvedNoteRef,
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
        assert_eq!(value["title_updated_at"], 10);
        assert_eq!(value["tags_updated_at"], 11);
        assert_eq!(value["publish_type"], "view");
        assert_eq!(value["published_at"], 13);
        assert_eq!(value["patch_path"], "notes/internal-id.md");
        assert!(value.get("folder_paths").is_none());
        assert!(value.get("titleUpdatedAt").is_none());
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
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/internal-id",
            200,
            r#"{"id":"internal-id","title":"Direct","content":"body"}"#,
        )]);
        let output = super::get_note(
            &fixture.client(),
            &crate::fixture::scratch_files(),
            GetNoteInput {
                workspace: Workspace::Personal,
                note_ref: Some("internal-id".to_owned()),
                refresh: false,
                local_path: None,
            },
        )
        .await
        .expect("direct get should succeed")
        .expect("direct reference should resolve");
        let super::GetNoteOutput::Note(output) = output else {
            panic!("a note_ref should return the note");
        };
        assert_eq!(output.id, "internal-id");
        fixture.finish();
    }

    #[tokio::test]
    async fn scoped_reference_has_a_bounded_discovery_then_item_budget() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"user","name":"User","userPath":"alice"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"resolved-id","title":"Note","permalink":"slug"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/resolved-id",
                200,
                r#"{"id":"resolved-id","title":"Note","content":"body"}"#,
            ),
        ]);
        let output = super::get_note(
            &fixture.client(),
            &crate::fixture::scratch_files(),
            GetNoteInput {
                workspace: Workspace::Personal,
                note_ref: Some("https://hackmd.io/@alice/slug".to_owned()),
                refresh: false,
                local_path: None,
            },
        )
        .await
        .expect("scoped get should succeed")
        .expect("scoped reference should resolve");
        let super::GetNoteOutput::Note(output) = output else {
            panic!("a note_ref should return the note");
        };
        assert_eq!(output.id, "resolved-id");
        fixture.finish();
    }

    #[tokio::test]
    async fn exactly_one_of_note_ref_and_local_path_is_taken() {
        let client = crate::client::HackmdClient::new(crate::config::Config::for_tests())
            .expect("client should build");
        let files = crate::fixture::scratch_files();
        for input in [
            serde_json::json!({}),
            serde_json::json!({"note_ref": "id", "local_path": "/tmp/note.md"}),
        ] {
            let input: GetNoteInput =
                serde_json::from_value(input).expect("input should deserialize");
            assert!(matches!(
                super::get_note(&client, &files, input).await,
                Err(super::GetNoteError::NoteRefOrLocalPath)
            ));
        }
        for input in [
            serde_json::json!({"local_path": "/tmp/note.md", "refresh": true}),
            serde_json::json!({"local_path": "/tmp/note.md", "team_path": "core"}),
        ] {
            let input: GetNoteInput =
                serde_json::from_value(input).expect("input should deserialize");
            assert!(matches!(
                super::get_note(&client, &files, input).await,
                Err(super::GetNoteError::SyncFilters)
            ));
        }
    }
}
