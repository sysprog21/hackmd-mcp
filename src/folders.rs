use std::collections::{BTreeMap, HashSet};

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::{
        CreateFolderRequest, FolderResponse, PatchField, UpdateFolderRequest,
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
    /// Internal folder ID from `hackmd_list_folders`. Required to change the
    /// folder's own fields; omit it with `child_order` to order the top-level
    /// folders.
    pub(crate) folder_id: Option<String>,
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
    /// Complete desired order of the direct child folders of `folder_id`, or
    /// of the top-level folders when `folder_id` is omitted. The parent is
    /// named by `folder_id`, not `parent_folder_id`. Works in personal
    /// workspaces too.
    #[serde(alias = "folder_ids")]
    pub(crate) child_order: Option<Vec<String>>,
}

impl UpdateFolderInput {
    /// Whether any of the folder's own fields is being changed, as opposed to
    /// only the order of its children.
    fn changes_fields(&self) -> bool {
        self.name.is_some()
            || self.description.is_specified()
            || self.icon.is_specified()
            || self.color.is_specified()
            || self.parent_folder_id.is_specified()
    }
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
pub(crate) struct UpdateFolderOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    /// The folder as read back, when its own fields changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) folder: Option<FolderResponse>,
    /// The order written, when `child_order` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) child_order: Option<ChildOrder>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChildOrder {
    /// The parent folder ID, or `root` for the top level.
    pub(crate) parent: String,
    pub(crate) folder_ids: Vec<String>,
}

#[derive(Debug, Error)]
pub(crate) enum FolderError {
    #[error("folder name must not be empty")]
    EmptyName,
    #[error("nothing to update: give the fields to change, child_order, or both")]
    NothingToUpdate,
    #[error("folder_id is required to change a folder's own fields")]
    MissingFolderId,
    #[error(
        "child_order orders the children of folder_id (omit it for the top level); parent_folder_id would move the folder, which HackMD does not support"
    )]
    OrderParentAsFolderId,
    #[error("folder {folder_id} was updated, but setting its child order failed: {source}")]
    OrderAfterUpdate {
        folder_id: String,
        #[source]
        source: Box<FolderError>,
    },
    #[error("folder {folder_id:?} does not exist in the selected workspace")]
    NotFound { folder_id: String },
    #[error("folder_ids contains duplicate folder ID {folder_id:?}")]
    DuplicateOrderId { folder_id: String },
    #[error(transparent)]
    Limit(#[from] InvalidLimit),
    #[error("HackMD accepted the folder update for {folder_id}, but read-back did not match")]
    ReadbackMismatch { folder_id: String },
    #[error(
        "HackMD accepted the order for {parent}, but it did not read back; another client may have written the folder order at the same time, so read it and set it again"
    )]
    OrderReadbackMismatch { parent: String },
    #[error("personal folder updates are unsupported: HackMD exposes PATCH only for team folders")]
    UnsupportedPersonalUpdate,
    #[error("folder moves are unsupported: HackMD accepts parent_folder_id but ignores it")]
    UnsupportedFolderMove,
    #[error(transparent)]
    Api(#[from] HackmdError),
}

impl crate::reply::ToolError for FolderError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::EmptyName
            | Self::NothingToUpdate
            | Self::MissingFolderId
            | Self::OrderParentAsFolderId
            | Self::DuplicateOrderId { .. }
            | Self::Limit(..)
            | Self::UnsupportedPersonalUpdate
            | Self::UnsupportedFolderMove => ErrorKind::InvalidInput,
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::OrderAfterUpdate { .. } => ErrorKind::PartialWrite,
            Self::ReadbackMismatch { .. } | Self::OrderReadbackMismatch { .. } => {
                ErrorKind::Readback
            }
            Self::Api(error) => error.kind(),
        }
    }
}

async fn set_child_order(
    client: &HackmdClient,
    workspace: &Workspace,
    parent: Option<&str>,
    folder_ids: Vec<String>,
) -> Result<ChildOrder, FolderError> {
    let parent = parent.unwrap_or("root").to_owned();

    // HackMD stores the order as one map and offers no conditional write, so
    // this is read-modify-write. The window cannot be closed from here, but a
    // write that was itself overwritten can be seen: the read-back must show
    // this parent's entry as written, or the caller is told to try again.
    let mut order: BTreeMap<String, Vec<String>> = client.get_folder_order(workspace).await?;
    order.insert(parent.clone(), folder_ids.clone());
    client.set_folder_order(workspace, &order).await?;
    client
        .poll_readback(
            0,
            || client.get_folder_order(workspace),
            // An empty order may come back as no entry at all.
            |current| {
                current
                    .get(&parent)
                    .map_or(folder_ids.is_empty(), |ids| *ids == folder_ids)
            },
        )
        .await?
        .confirmed_or(|| FolderError::OrderReadbackMismatch {
            parent: parent.clone(),
        })?;
    Ok(ChildOrder { parent, folder_ids })
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
) -> Result<UpdateFolderOutput, FolderError> {
    // How the retired order tool named the parent. Said plainly here rather
    // than as a folder-move refusal, which would not explain what to send.
    if input.child_order.is_some()
        && input.folder_id.is_none()
        && input.parent_folder_id.is_specified()
    {
        return Err(FolderError::OrderParentAsFolderId);
    }
    let changes_fields = input.changes_fields();
    if !changes_fields && input.child_order.is_none() {
        return Err(FolderError::NothingToUpdate);
    }

    // Checked before anything is written, so a bad order cannot fail the call
    // after the fields already changed.
    if let Some(folder_ids) = &input.child_order {
        ensure_distinct(folder_ids)?;
    }
    let UpdateFolderInput {
        workspace,
        folder_id,
        name,
        description,
        icon,
        color,
        parent_folder_id,
        child_order,
    } = input;

    let folder = if changes_fields {
        let folder_id = folder_id.as_deref().ok_or(FolderError::MissingFolderId)?;
        if name.as_deref().is_some_and(|name| name.trim().is_empty()) {
            return Err(FolderError::EmptyName);
        }
        if matches!(workspace, Workspace::Personal) {
            return Err(FolderError::UnsupportedPersonalUpdate);
        }
        if parent_folder_id.is_specified() {
            return Err(FolderError::UnsupportedFolderMove);
        }
        let payload = UpdateFolderRequest {
            name,
            description: description.into_request(),
            icon: icon.into_request(),
            color: color.into_request(),
        };
        Some(update_fields(client, &workspace, folder_id, &payload).await?)
    } else {
        None
    };
    let child_order = match child_order {
        Some(folder_ids) => Some(
            set_child_order(client, &workspace, folder_id.as_deref(), folder_ids)
                .await
                // The fields are already written: say so, or a retry would look
                // like it had to redo them.
                .map_err(|source| match &folder {
                    Some(folder) => FolderError::OrderAfterUpdate {
                        folder_id: folder.id.clone(),
                        source: Box::new(source),
                    },
                    None => source,
                })?,
        ),
        None => None,
    };
    Ok(UpdateFolderOutput {
        workspace,
        folder,
        child_order,
    })
}

fn ensure_distinct(folder_ids: &[String]) -> Result<(), FolderError> {
    let mut seen = HashSet::new();
    match folder_ids.iter().find(|folder_id| !seen.insert(*folder_id)) {
        Some(folder_id) => Err(FolderError::DuplicateOrderId {
            folder_id: folder_id.clone(),
        }),
        None => Ok(()),
    }
}

/// Writes a folder's own fields and waits until a read shows them.
async fn update_fields(
    client: &HackmdClient,
    workspace: &Workspace,
    folder_id: &str,
    payload: &UpdateFolderRequest,
) -> Result<FolderResponse, FolderError> {
    client.update_folder(workspace, folder_id, payload).await?;
    client
        .poll_readback(
            0,
            || client.get_folder(workspace, folder_id),
            |folder| folder_matches_update(folder, payload),
        )
        .await?
        .confirmed_or(|| FolderError::ReadbackMismatch {
            folder_id: folder_id.to_owned(),
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
        CreateFolderInput, DeleteFolderInput, FolderError, FolderWorkspaceInput, UpdateFolderInput,
        create_folder, delete_folder, list_folders, update_folder,
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

    fn root_order(folder_ids: &[String]) -> UpdateFolderInput {
        serde_json::from_value(json!({ "child_order": folder_ids }))
            .expect("order input should deserialize")
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
            "team_path": "team/path",
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
            "team_path": "team",
            "folder_id": "folder",
            "name": "New"
        }))
        .expect("folder update should deserialize");
        let output = update_folder(&fixture.client(), input)
            .await
            .expect("eventual folder update should succeed");
        assert_eq!(output.folder.expect("fields changed").name, "New");
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
            "team_path": "team",
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
            Scenario::new(
                "GET",
                "/v1/folders/folder-order",
                200,
                r#"{"root":["b","a"],"other":["keep"]}"#,
            ),
        ]);
        let output = update_folder(
            &fixture.client(),
            root_order(&["b".to_owned(), "a".to_owned()]),
        )
        .await
        .expect("order should update");
        let order = output.child_order.expect("the order should be reported");
        assert_eq!(order.parent, "root");
        assert!(output.folder.is_none());
        fixture.finish();
    }

    #[tokio::test]
    async fn update_refuses_what_it_cannot_do_before_any_request() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        for (input, expected) in [
            (json!({"folder_id": "f"}), "nothing to update"),
            (
                json!({"team_path": "t", "name": "N"}),
                "folder_id is required",
            ),
            (
                json!({"parent_folder_id": null, "folder_ids": ["a"]}),
                "child_order orders the children of folder_id",
            ),
            (
                json!({"child_order": ["a", "b", "a"]}),
                "folder_ids contains duplicate folder ID",
            ),
        ] {
            let input: UpdateFolderInput =
                serde_json::from_value(input).expect("input should deserialize");
            let error = update_folder(&client, input)
                .await
                .expect_err("the input should be refused");
            assert!(error.to_string().starts_with(expected), "{error}");
        }
    }

    #[tokio::test]
    async fn child_order_under_a_folder_uses_that_folder_as_parent() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/folders/folder-order", 200, r#"{"root":["p"]}"#),
            Scenario::new("PUT", "/v1/folders/folder-order", 204, "")
                .expect_body("the order under p", |body| {
                    body == r#"{"order":{"p":["b","a"],"root":["p"]}}"#
                }),
            Scenario::new(
                "GET",
                "/v1/folders/folder-order",
                200,
                r#"{"root":["p"],"p":["b","a"]}"#,
            ),
        ]);
        let input = serde_json::from_value(json!({"folder_id": "p", "child_order": ["b", "a"]}))
            .expect("input should deserialize");
        let output = update_folder(&fixture.client(), input)
            .await
            .expect("order should update");
        assert_eq!(output.child_order.expect("order reported").parent, "p");
        fixture.finish();
    }

    #[tokio::test]
    async fn an_order_failing_after_the_fields_changed_is_a_partial_write() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("PATCH", "/v1/teams/t/folders/f", 202, ""),
            Scenario::new(
                "GET",
                "/v1/teams/t/folders/f",
                200,
                r#"{"id":"f","name":"New"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams/t/folders/folder-order",
                500,
                r#"{"error":"down"}"#,
            ),
        ]);
        let input = serde_json::from_value(json!({
            "team_path": "t", "folder_id": "f", "name": "New", "child_order": ["a"]
        }))
        .expect("input should deserialize");
        let error = update_folder(&fixture.client_without_retry("fixture-token"), input)
            .await
            .expect_err("the order step should fail");
        assert!(matches!(error, FolderError::OrderAfterUpdate { .. }));
        assert_eq!(
            crate::reply::ToolError::kind(&error),
            crate::reply::ErrorKind::PartialWrite
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn an_order_overwritten_by_another_client_is_reported() {
        // Another client's full-map PUT lands just after this one and puts the
        // old root order back.
        let fixture = SequenceServer::spawn_repeating([
            (200, r#"{"root":["old"]}"#),
            (204, ""),
            (200, r#"{"root":["old"],"other":["theirs"]}"#),
        ]);
        let error = update_folder(
            &fixture.client(),
            root_order(&["b".to_owned(), "a".to_owned()]),
        )
        .await
        .expect_err("a lost order must not be reported as set");
        assert!(
            matches!(error, FolderError::OrderReadbackMismatch { ref parent } if parent == "root")
        );
        assert_eq!(
            crate::reply::ToolError::kind(&error),
            crate::reply::ErrorKind::Readback
        );
    }

    #[tokio::test]
    async fn an_emptied_order_that_reads_back_as_no_entry_is_confirmed() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/folders/folder-order", 200, r#"{"root":["a"]}"#),
            Scenario::new("PUT", "/v1/folders/folder-order", 204, ""),
            Scenario::new("GET", "/v1/folders/folder-order", 200, "{}"),
        ]);
        update_folder(&fixture.client(), root_order(&[]))
            .await
            .expect("an emptied order should confirm");
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
