use std::collections::{BTreeMap, HashMap, HashSet};

use rmcp::schemars;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::{CreateFolderRequest, FolderResponse, PayloadError, UpdateFolderRequest},
    models::Workspace,
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FolderWorkspaceInput {
    /// Personal account or team workspace.
    #[serde(default)]
    pub(crate) workspace: Workspace,
    /// Maximum folders returned (default 20, maximum 100).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub(crate) limit: usize,
    /// Number of folders to skip in API order.
    #[serde(default)]
    pub(crate) offset: usize,
}

const fn default_limit() -> usize {
    crate::list_notes::DEFAULT_LIMIT
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FolderRefInput {
    /// Personal account or team workspace.
    #[serde(default)]
    pub(crate) workspace: Workspace,
    /// Internal folder ID.
    pub(crate) folder_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateFolderInput {
    /// Personal account or team workspace.
    #[serde(default)]
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

#[derive(Debug, Default)]
enum NullableString {
    #[default]
    Unspecified,
    Set(Option<String>),
}

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<NullableString, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(NullableString::Set)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateFolderInput {
    /// Personal account or team workspace.
    #[serde(default)]
    pub(crate) workspace: Workspace,
    pub(crate) folder_id: String,
    /// New non-empty name; unlike other fields, name cannot be null.
    pub(crate) name: Option<String>,
    /// New description, null to clear, or omit to preserve.
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    #[schemars(with = "Option<String>")]
    description: NullableString,
    /// New icon, null to clear, or omit to preserve.
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    #[schemars(with = "Option<String>")]
    icon: NullableString,
    /// New color, null to clear, or omit to preserve.
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    #[schemars(with = "Option<String>")]
    color: NullableString,
    /// New parent, null for root, or omit to preserve.
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    #[schemars(with = "Option<String>")]
    parent_folder_id: NullableString,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteFolderInput {
    /// Personal account or team workspace.
    #[serde(default)]
    pub(crate) workspace: Workspace,
    pub(crate) folder_id: String,
    /// Required only when the folder has child folders.
    #[serde(default)]
    pub(crate) confirm: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetFolderOrderInput {
    /// Personal account or team workspace.
    #[serde(default)]
    pub(crate) workspace: Workspace,
    /// Parent folder ID. Omit or use null to order top-level folders.
    pub(crate) parent_folder_id: Option<String>,
    /// Complete desired order for this parent's direct child folders.
    pub(crate) folder_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderListOutput {
    pub(crate) workspace: Workspace,
    pub(crate) total: usize,
    pub(crate) count: usize,
    pub(crate) offset: usize,
    pub(crate) has_more: bool,
    pub(crate) next_offset: Option<usize>,
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
    pub(crate) workspace: Workspace,
    pub(crate) folder: FolderResponse,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeleteFolderOutput {
    pub(crate) workspace: Workspace,
    pub(crate) folder_id: String,
    pub(crate) deleted: bool,
    pub(crate) child_count: usize,
    pub(crate) confirmation_required: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderOrderOutput {
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
    #[error("folder {folder_id:?} cannot be moved into itself or one of its descendants")]
    MoveCycle { folder_id: String },
    #[error("folder hierarchy contains a pre-existing parent cycle at {folder_id:?}")]
    InvalidHierarchy { folder_id: String },
    #[error("folder_ids contains duplicate folder ID {folder_id:?}")]
    DuplicateOrderId { folder_id: String },
    #[error("limit must be between 1 and 100")]
    InvalidLimit,
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
    if !(1..=crate::list_notes::MAX_LIMIT).contains(&input.limit) {
        return Err(FolderError::InvalidLimit);
    }
    let folders = client.list_folders(&input.workspace).await?;
    let total = folders.len();
    let folders = folders
        .into_iter()
        .skip(input.offset)
        .take(input.limit)
        .map(|folder| FolderSummary {
            id: folder.id,
            name: folder.name,
            description: folder.description,
            icon: folder.icon,
            color: folder.color,
            parent_folder_id: folder.parent_folder_id,
        })
        .collect::<Vec<_>>();
    let count = folders.len();
    let next = input.offset.saturating_add(count);
    let has_more = next < total;
    Ok(FolderListOutput {
        workspace: input.workspace,
        total,
        count,
        offset: input.offset,
        has_more,
        next_offset: has_more.then_some(next),
        folders,
    })
}

pub(crate) async fn get_folder(
    client: &HackmdClient,
    input: FolderRefInput,
) -> Result<FolderOutput, FolderError> {
    let folder = client
        .get_folder(&input.workspace, &input.folder_id)
        .await?;
    Ok(FolderOutput {
        workspace: input.workspace,
        folder,
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
    let parent_folder_id = into_patch_field(input.parent_folder_id);
    let payload = UpdateFolderRequest {
        name: input.name,
        description: into_patch_field(input.description),
        icon: into_patch_field(input.icon),
        color: into_patch_field(input.color),
        parent_folder_id: parent_folder_id.clone(),
    };
    payload.validate()?;
    if let Some(Some(parent_id)) = parent_folder_id {
        let folders = client.list_folders(&input.workspace).await?;
        validate_move(&folders, &input.folder_id, &parent_id)?;
    }
    client
        .update_folder(&input.workspace, &input.folder_id, &payload)
        .await?;
    let folder = client
        .get_folder(&input.workspace, &input.folder_id)
        .await?;
    Ok(FolderOutput {
        workspace: input.workspace,
        folder,
    })
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

#[allow(
    clippy::option_option,
    reason = "outer None omits the PATCH field; inner None explicitly clears it"
)]
fn into_patch_field(value: NullableString) -> Option<Option<String>> {
    match value {
        NullableString::Unspecified => None,
        NullableString::Set(value) => Some(value),
    }
}

fn validate_move(
    folders: &[FolderResponse],
    folder_id: &str,
    parent_id: &str,
) -> Result<(), FolderError> {
    let parents = folders
        .iter()
        .map(|folder| (folder.id.as_str(), folder.parent_folder_id.as_deref()))
        .collect::<HashMap<_, _>>();
    if !parents.contains_key(folder_id) {
        return Err(FolderError::NotFound {
            folder_id: folder_id.to_owned(),
        });
    }
    if !parents.contains_key(parent_id) {
        return Err(FolderError::NotFound {
            folder_id: parent_id.to_owned(),
        });
    }
    let mut current = Some(parent_id);
    let mut seen = HashSet::new();
    while let Some(id) = current {
        if id == folder_id {
            return Err(FolderError::MoveCycle {
                folder_id: folder_id.to_owned(),
            });
        }
        if !seen.insert(id) {
            return Err(FolderError::InvalidHierarchy {
                folder_id: id.to_owned(),
            });
        }
        current = parents.get(id).copied().flatten();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CreateFolderInput, DeleteFolderInput, FolderError, FolderWorkspaceInput, NullableString,
        SetFolderOrderInput, UpdateFolderInput, create_folder, delete_folder, list_folders,
        set_folder_order, update_folder, validate_move,
    };
    use crate::{client::HackmdClient, config::Config, dto::FolderResponse, models::Workspace};

    fn fixture_client(server: &crate::test_support::SequenceServer) -> HackmdClient {
        HackmdClient::new(Config::for_loopback_test(
            &server.api_url,
            Some("fixture-token"),
        ))
        .expect("fixture client should build")
    }

    fn folder(id: &str, parent: Option<&str>) -> FolderResponse {
        serde_json::from_value(json!({
            "id": id,
            "name": id,
            "parentFolderId": parent
        }))
        .expect("folder fixture should deserialize")
    }

    #[test]
    fn update_fields_distinguish_absent_null_and_value() {
        let absent: UpdateFolderInput = serde_json::from_value(json!({"folder_id": "id"}))
            .expect("absent fields should deserialize");
        assert!(matches!(absent.color, NullableString::Unspecified));
        let clear: UpdateFolderInput = serde_json::from_value(json!({
            "folder_id": "id",
            "color": null
        }))
        .expect("null should deserialize");
        assert!(matches!(clear.color, NullableString::Set(None)));
        let set: UpdateFolderInput = serde_json::from_value(json!({
            "folder_id": "id",
            "color": "#fff"
        }))
        .expect("value should deserialize");
        assert!(matches!(set.color, NullableString::Set(Some(value)) if value == "#fff"));
    }

    #[test]
    fn move_validation_rejects_self_descendant_and_invalid_existing_cycle() {
        let tree = [folder("root", None), folder("child", Some("root"))];
        assert!(matches!(
            validate_move(&tree, "root", "root"),
            Err(FolderError::MoveCycle { .. })
        ));
        assert!(matches!(
            validate_move(&tree, "root", "child"),
            Err(FolderError::MoveCycle { .. })
        ));
        let cyclic = [
            folder("a", Some("b")),
            folder("b", Some("a")),
            folder("c", None),
        ];
        assert!(matches!(
            validate_move(&cyclic, "c", "a"),
            Err(FolderError::InvalidHierarchy { .. })
        ));
    }

    #[tokio::test]
    async fn root_create_omits_parent_and_team_create_is_preflighted() {
        let personal = crate::test_support::SequenceServer::spawn([(
            201,
            r#"{"id":"root/id","name":"Root"}"#,
        )]);
        let output = create_folder(
            &fixture_client(&personal),
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
        let requests = personal.finish();
        assert!(requests[0].starts_with("POST /v1/folders HTTP/1.1\r\n"));
        assert!(requests[0].ends_with(r#"{"name":"Root"}"#));
        assert!(!requests[0].contains("parentFolderId"));

        let team = crate::test_support::SequenceServer::spawn([
            (200, r#"[{"id":"t","name":"Team","path":"team/path"}]"#),
            (201, r#"{"id":"nested","name":"Nested"}"#),
        ]);
        create_folder(
            &fixture_client(&team),
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
        let requests = team.finish();
        assert!(requests[0].starts_with("GET /v1/teams HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("POST /v1/teams/team%2Fpath/folders HTTP/1.1\r\n"));
        assert!(requests[1].ends_with(r#"{"name":"Nested","parentFolderId":"parent/id"}"#));
    }

    #[tokio::test]
    async fn update_clears_fields_then_reads_encoded_folder_back() {
        let fixture = crate::test_support::SequenceServer::spawn([
            (202, ""),
            (200, r#"{"id":"folder/id","name":"Moved"}"#),
        ]);
        let input = serde_json::from_value(json!({
            "folder_id": "folder/id",
            "description": null,
            "parent_folder_id": null
        }))
        .expect("tri-state update should deserialize");
        update_folder(&fixture_client(&fixture), input)
            .await
            .expect("folder update should read back");
        let requests = fixture.finish();
        assert!(requests[0].starts_with("PATCH /v1/folders/folder%2Fid HTTP/1.1\r\n"));
        assert!(requests[0].ends_with(r#"{"description":null,"parentFolderId":null}"#));
        assert!(requests[1].starts_with("GET /v1/folders/folder%2Fid HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn nonempty_delete_requires_confirmation_without_mutation() {
        let fixture = crate::test_support::SequenceServer::spawn([(
            200,
            r#"[{"id":"parent","name":"Parent"},{"id":"child","name":"Child","parentFolderId":"parent"}]"#,
        )]);
        let output = delete_folder(
            &fixture_client(&fixture),
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
        assert_eq!(fixture.finish().len(), 1);
    }

    #[tokio::test]
    async fn folder_order_merge_preserves_unrelated_entries() {
        let fixture = crate::test_support::SequenceServer::spawn([
            (200, r#"{"root":["old"],"other":["keep"]}"#),
            (204, ""),
        ]);
        let output = set_folder_order(
            &fixture_client(&fixture),
            SetFolderOrderInput {
                workspace: Workspace::Personal,
                parent_folder_id: None,
                folder_ids: vec!["b".to_owned(), "a".to_owned()],
            },
        )
        .await
        .expect("order should update");
        assert_eq!(output.parent, "root");
        let requests = fixture.finish();
        assert!(requests[0].starts_with("GET /v1/folders/folder-order HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("PUT /v1/folders/folder-order HTTP/1.1\r\n"));
        assert!(requests[1].ends_with(r#"{"order":{"other":["keep"],"root":["b","a"]}}"#));
    }

    #[tokio::test]
    async fn folder_list_is_slim_and_explicitly_paginated() {
        let fixture = crate::test_support::SequenceServer::spawn([(
            200,
            r#"[
                {"id":"a","name":"A","createdAt":1,"updatedAt":2},
                {"id":"b","name":"B","description":"second","createdAt":3,"updatedAt":4},
                {"id":"c","name":"C"}
            ]"#,
        )]);
        let output = list_folders(
            &fixture_client(&fixture),
            FolderWorkspaceInput {
                workspace: Workspace::Personal,
                limit: 1,
                offset: 1,
            },
        )
        .await
        .expect("folder list should succeed");
        assert_eq!(output.total, 3);
        assert_eq!(output.count, 1);
        assert_eq!(output.offset, 1);
        assert!(output.has_more);
        assert_eq!(output.next_offset, Some(2));
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
        assert!(matches!(error, FolderError::InvalidLimit));
    }
}
