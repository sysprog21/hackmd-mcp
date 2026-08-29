//! Folder tools, including the workspace-wide folder order.

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use super::HackmdServer;
use crate::{
    folders::{
        CreateFolderInput, DeleteFolderInput, FolderRefInput, FolderWorkspaceInput,
        SetFolderOrderInput, UpdateFolderInput,
    },
    reply,
};

#[tool_router(router = folder_router, vis = "pub(crate)")]
impl HackmdServer {
    #[tool(
        name = "hackmd_list_folders",
        description = "List folders in a personal or team HackMD workspace.",
        annotations(
            title = "List HackMD Folders",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn list_folders(
        &self,
        Parameters(input): Parameters<FolderWorkspaceInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::list_folders(&self.client, input).await {
            Ok(output) => reply::structured(
                format!(
                    "Found {} HackMD folder(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                &output,
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_get_folder",
        description = "Get one folder by internal ID in a personal or team HackMD workspace.",
        annotations(
            title = "Get HackMD Folder",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn get_folder(
        &self,
        Parameters(input): Parameters<FolderRefInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::get_folder(&self.client, input).await {
            Ok(output) => reply::structured(
                format!("Fetched HackMD folder {}", output.folder.id),
                &output,
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_create_folder",
        description = "Create a root or nested folder in a personal or verified team HackMD workspace.",
        annotations(
            title = "Create HackMD Folder",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn create_folder(
        &self,
        Parameters(input): Parameters<CreateFolderInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::create_folder(&self.client, input).await {
            Ok(output) => reply::structured(
                format!("Created HackMD folder {}", output.folder.id),
                &output,
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_update_folder",
        description = "Update team folder metadata and read back until PATCH is visible. Personal folder PATCH and all folder moves are unsupported by HackMD.",
        annotations(
            title = "Update HackMD Folder",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn update_folder(
        &self,
        Parameters(input): Parameters<UpdateFolderInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::update_folder(&self.client, input).await {
            Ok(output) => reply::structured(
                format!("Updated HackMD folder {}", output.folder.id),
                &output,
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_delete_folder",
        description = "Delete a folder. A non-empty folder is unchanged unless the same request supplies confirm: true.",
        annotations(
            title = "Delete HackMD Folder",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn delete_folder(
        &self,
        Parameters(input): Parameters<DeleteFolderInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::delete_folder(&self.client, input).await {
            Ok(output) => {
                let summary = if output.deleted {
                    format!("Deleted HackMD folder {}", output.folder_id)
                } else {
                    format!(
                        "Folder {} has {} child folder(s); confirm deletion",
                        output.folder_id, output.child_count
                    )
                };
                reply::structured(summary, &output)
            }
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_set_folder_order",
        description = "Set the direct-child order for one parent while preserving every unrelated entry in HackMD's whole folder-order map. Omit parent_folder_id for root.",
        annotations(
            title = "Set HackMD Folder Order",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn set_folder_order(
        &self,
        Parameters(input): Parameters<SetFolderOrderInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::set_folder_order(&self.client, input).await {
            Ok(output) => reply::structured(
                format!("Set HackMD folder order for {}", output.parent),
                &output,
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }
}
