use std::collections::{BTreeMap, HashSet};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::{
        CreateFolderRequest, FolderResponse, PatchField, PayloadError, UpdateFolderRequest,
        deserialize_patch_field,
    },
    models::Workspace,
    paging::{InvalidLimit, PageMeta, default_limit, paginate, validate_limit},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FolderWorkspaceInput {
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Maximum folders returned (default 20, maximum 100).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub(crate) limit: usize,
    /// Number of folders to skip in API order.
    #[serde(default)]
    pub(crate) offset: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateFolderInput {
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Non-empty display name.
    #[schemars(length(min = 1))]
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) icon: Option<String>,
    pub(crate) color: Option<String>,
    /// Parent folder ID. Omit for a root folder; null is not sent on create.
    pub(crate) parent_folder_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateFolderInput {
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Internal folder ID from `hackmd_list_folders`.
    pub(crate) folder_id: String,
    /// New non-empty name; unlike other fields, name cannot be null.
    pub(crate) name: Option<String>,
    /// New description, null to clear, or omit to preserve.
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    #[schemars(with = "Option<String>")]
    description: PatchField,
    /// New icon, null to clear, or omit to preserve.
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    #[schemars(with = "Option<String>")]
    icon: PatchField,
    /// New color, null to clear, or omit to preserve.
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    #[schemars(with = "Option<String>")]
    color: PatchField,
    /// Not supported: `HackMD` reports folder moves as successful while
    /// leaving the parent unchanged, so any value here is refused.
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    #[schemars(with = "Option<String>")]
    parent_folder_id: PatchField,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteFolderInput {
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Internal folder ID from `hackmd_list_folders`.
    pub(crate) folder_id: String,
    /// Required only when the folder has child folders.
    #[serde(default)]
    pub(crate) confirm: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetFolderOrderInput {
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Parent folder ID. Omit or use null to order top-level folders.
    pub(crate) parent_folder_id: Option<String>,
    /// Complete desired order for this parent's direct child folders.
    pub(crate) folder_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderListOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    #[serde(flatten)]
    pub(crate) meta: PageMeta,
    pub(crate) folders: Vec<FolderSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) icon: Option<String>,
    pub(crate) color: Option<String>,
    pub(crate) parent_folder_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) folder: FolderResponse,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeleteFolderOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) folder_id: String,
    pub(crate) deleted: bool,
    pub(crate) child_count: usize,
    pub(crate) confirmation_required: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderOrderOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) parent: String,
    pub(crate) folder_ids: Vec<String>,
}

#[derive(Debug, Error)]
pub(crate) enum FolderError {
    #[error("folder name must not be empty")]
    EmptyName,
    #[error("folder {folder_id:?} does not exist in the selected workspace")]
    NotFound { folder_id: String },
    #[error("folder_ids contains duplicate folder ID {folder_id:?}")]
    DuplicateOrderId { folder_id: String },
    #[error(transparent)]
    Limit(#[from] InvalidLimit),
    #[error("HackMD accepted the folder update for {folder_id}, but read-back did not match")]
    ReadbackMismatch { folder_id: String },
    #[error("personal folder updates are unsupported: HackMD exposes PATCH only for team folders")]
    UnsupportedPersonalUpdate,
    #[error("folder moves are unsupported: HackMD accepts parent_folder_id but ignores it")]
    UnsupportedFolderMove,
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn set_folder_order(
    client: &HackmdClient,
    input: SetFolderOrderInput,
) -> Result<FolderOrderOutput, FolderError> {
    let mut seen = HashSet::new();
    for folder_id in &input.folder_ids {
        if !seen.insert(folder_id) {
            return Err(FolderError::DuplicateOrderId {
                folder_id: folder_id.clone(),
            });
        }
    }
    let parent = input.parent_folder_id.unwrap_or_else(|| "root".to_owned());
    let mut order: BTreeMap<String, Vec<String>> =
        client.get_folder_order(&input.workspace).await?;
    order.insert(parent.clone(), input.folder_ids.clone());
    client.set_folder_order(&input.workspace, &order).await?;
    Ok(FolderOrderOutput {
        workspace: input.workspace,
        parent,
        folder_ids: input.folder_ids,
    })
}

pub(crate) async fn list_folders(
    client: &HackmdClient,
    input: FolderWorkspaceInput,
) -> Result<FolderListOutput, FolderError> {
    validate_limit(input.limit)?;
    let folders = client
        .list_folders(&input.workspace)
        .await?
        .into_iter()
        .map(|folder| FolderSummary {
            id: folder.id,
            name: folder.name,
            description: folder.description,
            icon: folder.icon,
            color: folder.color,
            parent_folder_id: folder.parent_folder_id,
        })
        .collect();
    let (folders, meta) = paginate(folders, input.offset, input.limit);
    Ok(FolderListOutput {
        workspace: input.workspace,
        meta,
        folders,
    })
}

pub(crate) async fn create_folder(
    client: &HackmdClient,
    input: CreateFolderInput,
) -> Result<FolderOutput, FolderError> {
    if input.name.trim().is_empty() {
        return Err(FolderError::EmptyName);
    }
    client.ensure_team_exists(&input.workspace).await?;
    let folder = client
        .create_folder(
            &input.workspace,
            &CreateFolderRequest {
                name: input.name,
                description: input.description,
                icon: input.icon,
                color: input.color,
                parent_folder_id: input.parent_folder_id,
            },
        )
        .await?;
    Ok(FolderOutput {
        workspace: input.workspace,
        folder,
    })
}

pub(crate) async fn update_folder(
    client: &HackmdClient,
    input: UpdateFolderInput,
) -> Result<FolderOutput, FolderError> {
    if input
        .name
        .as_deref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return Err(FolderError::EmptyName);
    }
    if matches!(input.workspace, Workspace::Personal) {
        return Err(FolderError::UnsupportedPersonalUpdate);
    }
    if input.parent_folder_id.into_request().is_some() {
        return Err(FolderError::UnsupportedFolderMove);
    }
    let payload = UpdateFolderRequest {
        name: input.name,
        description: input.description.into_request(),
        icon: input.icon.into_request(),
        color: input.color.into_request(),
    };
    payload.validate()?;
    client
        .update_folder(&input.workspace, &input.folder_id, &payload)
        .await?;
    let folder = crate::client::poll_readback(
        || client.get_folder(&input.workspace, &input.folder_id),
        |folder| folder_matches_update(folder, &payload),
    )
    .await?;
    if !folder.confirmed {
        return Err(FolderError::ReadbackMismatch {
            folder_id: input.folder_id.clone(),
        });
    }
    let folder = folder.value;
    Ok(FolderOutput {
        workspace: input.workspace,
        folder,
    })
}

fn folder_matches_update(folder: &FolderResponse, update: &UpdateFolderRequest) -> bool {
    update.name.as_ref().is_none_or(|name| folder.name == *name)
        && update
            .description
            .as_ref()
            .is_none_or(|description| folder.description == *description)
        && update.icon.as_ref().is_none_or(|icon| folder.icon == *icon)
        && update
            .color
            .as_ref()
            .is_none_or(|color| folder.color == *color)
}

pub(crate) async fn delete_folder(
    client: &HackmdClient,
    input: DeleteFolderInput,
) -> Result<DeleteFolderOutput, FolderError> {
    let folders = client.list_folders(&input.workspace).await?;
    if !folders.iter().any(|folder| folder.id == input.folder_id) {
        return Err(FolderError::NotFound {
            folder_id: input.folder_id,
        });
    }
    let child_count = folders
        .iter()
        .filter(|folder| folder.parent_folder_id.as_deref() == Some(&input.folder_id))
        .count();
    if child_count > 0 && !input.confirm {
        return Ok(DeleteFolderOutput {
            workspace: input.workspace,
            folder_id: input.folder_id,
            deleted: false,
            child_count,
            confirmation_required: true,
        });
    }
    client
        .delete_folder(&input.workspace, &input.folder_id)
        .await?;
    Ok(DeleteFolderOutput {
        workspace: input.workspace,
        folder_id: input.folder_id,
        deleted: true,
        child_count,
        confirmation_required: false,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CreateFolderInput, DeleteFolderInput, FolderError, FolderWorkspaceInput,
        SetFolderOrderInput, UpdateFolderInput, create_folder, delete_folder, list_folders,
        set_folder_order, update_folder,
    };
    use crate::{
        client::HackmdClient,
        config::Config,
        dto::PatchField,
        fixture::{Scenario, SequenceServer},
        models::Workspace,
    };

    #[test]
    fn update_fields_distinguish_absent_null_and_value() {
        let absent: UpdateFolderInput = serde_json::from_value(json!({"folder_id": "id"}))
            .expect("absent fields should deserialize");
        assert!(matches!(absent.color, PatchField::Unspecified));
        let clear: UpdateFolderInput = serde_json::from_value(json!({
            "folder_id": "id",
            "color": null
        }))
        .expect("null should deserialize");
        assert!(matches!(clear.color, PatchField::Set(None)));
        let set: UpdateFolderInput = serde_json::from_value(json!({
            "folder_id": "id",
            "color": "#fff"
        }))
        .expect("value should deserialize");
        assert!(matches!(set.color, PatchField::Set(Some(value)) if value == "#fff"));
    }

    #[tokio::test]
    async fn root_create_omits_parent_and_team_create_is_preflighted() {
        let personal = SequenceServer::spawn_scenarios([Scenario::new(
            "POST",
            "/v1/folders",
            201,
            r#"{"id":"root/id","name":"Root"}"#,
        )
        .expect_body("a root folder without parentFolderId", |body| {
            body == r#"{"name":"Root"}"#
        })]);
        let output = create_folder(
            &personal.client(),
            CreateFolderInput {
                workspace: Workspace::Personal,
                name: "Root".to_owned(),
                description: None,
                icon: None,
                color: None,
                parent_folder_id: None,
            },
        )
        .await
        .expect("root folder should create");
        assert_eq!(output.folder.id, "root/id");
        personal.finish();

        let team = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/teams",
                200,
                r#"[{"id":"t","name":"Team","path":"team/path"}]"#,
            ),
            Scenario::new(
                "POST",
                "/v1/teams/team%2Fpath/folders",
                201,
                r#"{"id":"nested","name":"Nested"}"#,
            )
            .expect_body("the nested folder and encoded parent", |body| {
                body == r#"{"name":"Nested","parentFolderId":"parent/id"}"#
            }),
        ]);
        create_folder(
            &team.client(),
            CreateFolderInput {
                workspace: Workspace::Team {
                    team_path: "team/path".to_owned(),
                },
                name: "Nested".to_owned(),
                description: None,
                icon: None,
                color: None,
                parent_folder_id: Some("parent/id".to_owned()),
            },
        )
        .await
        .expect("known team folder should create");
        team.finish();
    }

    #[tokio::test]
    async fn update_clears_nullable_metadata_then_reads_encoded_folder_back() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "PATCH",
                "/v1/teams/team%2Fpath/folders/folder%2Fid",
                202,
                "",
            )
            .expect_body("the cleared description", |body| {
                body == r#"{"description":null}"#
            }),
            Scenario::new(
                "GET",
                "/v1/teams/team%2Fpath/folders/folder%2Fid",
                200,
                r#"{"id":"folder/id","name":"Moved"}"#,
            ),
        ]);
        let input = serde_json::from_value(json!({
            "workspace": {"kind": "team", "team_path": "team/path"},
            "folder_id": "folder/id",
            "description": null
        }))
        .expect("tri-state update should deserialize");
        update_folder(&fixture.client(), input)
            .await
            .expect("folder update should read back");
        fixture.finish();
    }

    #[tokio::test]
    async fn update_polls_until_async_change_is_visible() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("PATCH", "/v1/teams/team/folders/folder", 202, ""),
            Scenario::new(
                "GET",
                "/v1/teams/team/folders/folder",
                200,
                r#"{"id":"folder","name":"Old"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams/team/folders/folder",
                200,
                r#"{"id":"folder","name":"New"}"#,
            ),
        ]);
        let input = serde_json::from_value(json!({
            "workspace": {"kind": "team", "team_path": "team"},
            "folder_id": "folder",
            "name": "New"
        }))
        .expect("folder update should deserialize");
        let output = update_folder(&fixture.client(), input)
            .await
            .expect("eventual folder update should succeed");
        assert_eq!(output.folder.name, "New");
        fixture.finish();
    }

    #[tokio::test]
    async fn personal_update_is_rejected_before_network_after_live_no_op() {
        let input = serde_json::from_value(json!({
            "folder_id": "folder",
            "parent_folder_id": null
        }))
        .expect("root move should deserialize");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        assert!(matches!(
            update_folder(&client, input).await,
            Err(FolderError::UnsupportedPersonalUpdate)
        ));
    }

    #[tokio::test]
    async fn team_move_is_rejected_before_network_after_live_no_op() {
        let input = serde_json::from_value(json!({
            "workspace": {"kind": "team", "team_path": "team"},
            "folder_id": "folder",
            "parent_folder_id": "destination"
        }))
        .expect("folder move should deserialize");
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        assert!(matches!(
            update_folder(&client, input).await,
            Err(FolderError::UnsupportedFolderMove)
        ));
    }

    #[tokio::test]
    async fn nonempty_delete_requires_confirmation_without_mutation() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/folders",
            200,
            r#"[{"id":"parent","name":"Parent"},{"id":"child","name":"Child","parentFolderId":"parent"}]"#,
        )]);
        let output = delete_folder(
            &fixture.client(),
            DeleteFolderInput {
                workspace: Workspace::Personal,
                folder_id: "parent".to_owned(),
                confirm: false,
            },
        )
        .await
        .expect("inspection should succeed");
        assert!(!output.deleted);
        assert_eq!(output.child_count, 1);
        assert!(output.confirmation_required);
        fixture.finish();
    }

    #[tokio::test]
    async fn folder_order_merge_preserves_unrelated_entries() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/folders/folder-order",
                200,
                r#"{"root":["old"],"other":["keep"]}"#,
            ),
            Scenario::new("PUT", "/v1/folders/folder-order", 204, "")
                .expect_body("the merged folder order", |body| {
                    body == r#"{"order":{"other":["keep"],"root":["b","a"]}}"#
                }),
        ]);
        let output = set_folder_order(
            &fixture.client(),
            SetFolderOrderInput {
                workspace: Workspace::Personal,
                parent_folder_id: None,
                folder_ids: vec!["b".to_owned(), "a".to_owned()],
            },
        )
        .await
        .expect("order should update");
        assert_eq!(output.parent, "root");
        fixture.finish();
    }

    #[tokio::test]
    async fn folder_list_is_slim_and_explicitly_paginated() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/folders",
            200,
            r#"[
                {"id":"a","name":"A","createdAt":1,"updatedAt":2},
                {"id":"b","name":"B","description":"second","createdAt":3,"updatedAt":4},
                {"id":"c","name":"C"}
            ]"#,
        )]);
        let output = list_folders(
            &fixture.client(),
            FolderWorkspaceInput {
                workspace: Workspace::Personal,
                limit: 1,
                offset: 1,
            },
        )
        .await
        .expect("folder list should succeed");
        assert_eq!(output.meta.total, 3);
        assert_eq!(output.meta.count, 1);
        assert_eq!(output.meta.offset, 1);
        assert!(output.meta.has_more);
        assert_eq!(output.meta.next_offset, Some(2));
        let value = serde_json::to_value(&output.folders[0]).expect("summary should serialize");
        assert_eq!(value["id"], "b");
        assert!(value.get("created_at").is_none());
        assert!(value.get("updated_at").is_none());
        fixture.finish();
    }

    #[tokio::test]
    async fn folder_list_rejects_invalid_limit_before_requesting() {
        let error = list_folders(
            &HackmdClient::new(Config::for_tests()).expect("client should build"),
            FolderWorkspaceInput {
                workspace: Workspace::Personal,
                limit: 0,
                offset: 0,
            },
        )
        .await
        .expect_err("zero limit should be rejected");
        assert!(matches!(error, FolderError::Limit(_)));
    }
}
