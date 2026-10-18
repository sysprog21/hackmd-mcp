use std::path::PathBuf;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::{
        CommentPermission, NotePermission, NotePublishType, NoteResponse,
        SimpleUserProfileResponse, SuggestEditPermission,
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
    /// workspace. With an `s/<shortId>` link, only this team is searched;
    /// omitted, the personal notes and then every team are.
    #[serde(
        default,
        rename = "team_path",
        alias = "workspace",
        deserialize_with = "crate::models::given"
    )]
    pub(crate) workspace: Option<Workspace>,
    /// Internal API ID, `hackmd.io/<id>`, `hackmd.io/@owner/slug` URL, or legacy
    /// `hackmd.io/s/<shortId>` share link. A trailing `/download` is ignored.
    /// Give this or `local_path`, not both.
    pub(crate) note_ref: Option<String>,
    /// Bypass the 60-second account and note-list caches when resolving an
    /// `@owner/slug` URL or `s/<shortId>` link.
    #[serde(default)]
    pub(crate) refresh: bool,
    /// Instead of `note_ref`: the absolute path of a Markdown file tracked by
    /// `hackmd_pull_note`. The result is then that file's sync state, compared
    /// three ways against its baseline and the remote note, without writing:
    /// `in_sync`, `local_changed`, `remote_changed`, or `conflict`, with the
    /// SHA-256 of each body.
    pub(crate) local_path: Option<PathBuf>,
    /// With `note_ref`, `false` leaves the body out of the result and keeps
    /// everything else, `body_hash` included, for a caller that wants only the
    /// metadata or the hash of many notes. `HackMD` still sends the whole note;
    /// this only keeps the body out of the agent's context.
    #[serde(default = "include_content_default")]
    pub(crate) include_content: bool,
}

const fn include_content_default() -> bool {
    true
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct NoteDetail {
    pub(crate) id: String,
    /// See [`note_url`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note_url: Option<String>,
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
    pub(crate) suggest_edit_permission: Option<SuggestEditPermission>,
}

#[derive(Debug, Error)]
pub(crate) enum GetNoteError {
    #[error("give either note_ref or local_path")]
    NoteRefOrLocalPath,
    #[error(
        "local_path is looked up in the local sync records; omit team_path, refresh, and include_content"
    )]
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
            if input.workspace.is_some() || input.refresh || !input.include_content {
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
    let response = client
        .get_note_retained(&note.workspace, &note.note_id)
        .await?;
    let mut detail = crate::local::offload(|| normalize_note(client.site_url(), note, response));
    if !input.include_content {
        detail.content = None;
    }
    Ok(Ok(GetNoteOutput::Note(Box::new(detail))))
}

/// The site link of a note, reported as `note_url`: `https://hackmd.io/<id>`
/// opens personal and team notes alike. It is a link, not an invite or proof
/// of public access. There is none without a known site (a custom API
/// endpoint), nor for an ID that is not one safe path segment.
pub(crate) fn note_url(site: Option<&Url>, note_id: &str) -> Option<String> {
    crate::client::append_segments(site?, &[note_id])
        .ok()
        .map(Into::into)
}

pub(crate) fn normalize_note(
    site: Option<&Url>,
    reference: ResolvedNoteRef,
    note: NoteResponse,
) -> NoteDetail {
    let patch_path = crate::note::patch::patch_path(&reference.workspace, &reference.note_id);
    NoteDetail {
        note_url: note_url(site, &note.id),
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
            .flatten()
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
        suggest_edit_permission: note.suggest_edit_permission,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{GetNoteInput, normalize_note, note_url};
    use crate::{
        dto::NoteResponse,
        fixture::{Scenario, SequenceServer},
        models::Workspace,
        note::reference::ResolvedNoteRef,
    };

    /// `null` is what clients send for an unset optional field, and older
    /// clients send the tagged personal object by default, so both mean "not
    /// given", as omission does. Both aliases read the same.
    #[test]
    fn a_null_team_path_is_the_same_as_none_given() {
        let team_path = |input: serde_json::Value| {
            serde_json::from_value::<GetNoteInput>(input)
                .expect("input should deserialize")
                .workspace
        };
        assert_eq!(team_path(json!({"note_ref": "id"})), None);
        assert_eq!(
            team_path(json!({"note_ref": "id", "team_path": null})),
            None
        );
        assert_eq!(
            team_path(json!({"note_ref": "id", "workspace": null})),
            None
        );
        assert_eq!(
            team_path(json!({"note_ref": "id", "workspace": {"kind": "personal"}})),
            None
        );
        let core = Some(Workspace::Team {
            team_path: "core".to_owned(),
        });
        assert_eq!(
            team_path(json!({"note_ref": "id", "team_path": "core"})),
            core
        );
        assert_eq!(
            team_path(
                json!({"note_ref": "id", "workspace": {"kind": "team", "team_path": "core"}})
            ),
            core
        );
    }

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
    fn note_links_encode_the_id_and_refuse_unsafe_ones() {
        let site = crate::fixture::site();
        let link = |id| note_url(Some(&site), id);
        assert_eq!(
            link("note-id").as_deref(),
            Some("https://hackmd.io/note-id")
        );
        assert_eq!(
            link("a/b?#%2F\\").as_deref(),
            Some("https://hackmd.io/a%2Fb%3F%23%252F%5C")
        );
        assert_eq!(link("é").as_deref(), Some("https://hackmd.io/%C3%A9"));
        for id in ["", " ", ".", "..", "a\nb"] {
            assert!(link(id).is_none(), "{id:?}");
        }
        assert!(note_url(None, "note-id").is_none());
    }

    #[test]
    fn personal_detail_has_exact_patch_path_and_flat_folder_ids() {
        let detail = normalize_note(
            Some(&crate::fixture::site()),
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
        assert_eq!(value["note_url"], "https://hackmd.io/internal-id");
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
            Some(&crate::fixture::site()),
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
        // Without include_content the body comes back.
        let input = serde_json::from_value(json!({"note_ref": "internal-id"}))
            .expect("input should deserialize");
        let output = super::get_note(&fixture.client(), &crate::fixture::scratch_files(), input)
            .await
            .expect("direct get should succeed")
            .expect("direct reference should resolve");
        let super::GetNoteOutput::Note(output) = output else {
            panic!("a note_ref should return the note");
        };
        assert_eq!(output.id, "internal-id");
        assert_eq!(output.content.as_deref(), Some("body"));
        fixture.finish();
    }

    /// Without the body the result still carries the hash of the body as read,
    /// so a caller can check many notes for changes without holding them.
    #[tokio::test]
    async fn without_content_the_body_hash_stays() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/internal-id",
            200,
            r#"{"id":"internal-id","title":"T","content":"body","readPermission":"guest"}"#,
        )]);
        let input = serde_json::from_value(json!({
            "note_ref": "internal-id",
            "include_content": false
        }))
        .expect("input should deserialize");
        let output = super::get_note(&fixture.client(), &crate::fixture::scratch_files(), input)
            .await
            .expect("get should succeed")
            .expect("reference should resolve");
        let super::GetNoteOutput::Note(output) = output else {
            panic!("a note_ref should return the note");
        };
        let value = serde_json::to_value(&output).expect("detail should serialize");
        assert!(value.get("content").is_none());
        assert_eq!(value["body_hash"], crate::hash::body_hash("body"));
        assert_eq!(value["read_permission"], "guest");
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
                workspace: None,
                note_ref: Some("https://hackmd.io/@alice/slug".to_owned()),
                refresh: false,
                local_path: None,
                include_content: true,
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
            serde_json::json!({"local_path": "/tmp/note.md", "include_content": false}),
        ] {
            let input: GetNoteInput =
                serde_json::from_value(input).expect("input should deserialize");
            let error = super::get_note(&client, &files, input)
                .await
                .expect_err("a sync filter should be refused");
            assert!(matches!(error, super::GetNoteError::SyncFilters));
            assert_eq!(
                error.to_string(),
                "local_path is looked up in the local sync records; omit team_path, refresh, and include_content"
            );
        }
    }
}
