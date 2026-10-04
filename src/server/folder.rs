//! Folder tools, including the workspace-wide folder order.

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use super::HackmdServer;
use crate::{
    folders::{CreateFolderInput, DeleteFolderInput, FolderWorkspaceInput, UpdateFolderInput},
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
        reply::respond(
            crate::folders::list_folders(&self.client, input).await,
            |output| {
                format!(
                    "Found {} HackMD folder(s); returned {}",
                    output.meta.total, output.meta.count
                )
            },
        )
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
        reply::respond(
            crate::folders::create_folder(&self.client, input).await,
            |output| format!("Created HackMD folder {}", output.folder.id),
        )
    }

    #[tool(
        name = "hackmd_update_folder",
        description = "Update team folder metadata, and/or set the order of a folder's direct child folders with child_order. folder_id names the folder whose fields change and whose children child_order orders; omit it with child_order to order the top level, which works in personal workspaces too. Every change is read back until visible. HackMD does not support personal folder metadata updates or folder moves; the folder order is one shared map, so an order another client overwrote is reported rather than claimed.",
        annotations(
            title = "Update HackMD Folder",
            read_only_hint = false,
            // A null clears a field for good, and child_order replaces part of
            // a map other clients share.
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn update_folder(
        &self,
        Parameters(input): Parameters<UpdateFolderInput>,
    ) -> rmcp::model::CallToolResult {
        reply::respond(
            crate::folders::update_folder(&self.client, input).await,
            |output| match (&output.folder, &output.child_order) {
                (Some(folder), Some(_)) => {
                    format!(
                        "Updated HackMD folder {} and set its child order",
                        folder.id
                    )
                }
                (Some(folder), None) => format!("Updated HackMD folder {}", folder.id),
                (None, order) => format!(
                    "Set HackMD folder order for {}",
                    order.as_ref().map_or("root", |order| order.parent.as_str())
                ),
            },
        )
    }

    #[tool(
        name = "hackmd_delete_folder",
        description = "Delete a folder. A folder with child folders is left unchanged unless the same request supplies confirm: true. Notes are not counted: HackMD list endpoints do not report folder membership, so check hackmd_get_note folder_ids first if that matters.",
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
        reply::respond(
            crate::folders::delete_folder(&self.client, input).await,
            |output| {
                if output.deleted {
                    format!("Deleted HackMD folder {}", output.folder_id)
                } else {
                    format!(
                        "Folder {} has {} child folder(s); confirm deletion",
                        output.folder_id, output.child_count
                    )
                }
            },
        )
    }
}
