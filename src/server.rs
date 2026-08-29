use std::sync::Arc;

use rmcp::{ServiceExt, tool_router};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool};
use serde::Deserialize;
use tracing::Instrument;

use crate::{
    client::HackmdClient,
    config::Config,
    dto::{ProfileResponse, TeamResponse},
    folders::{
        CreateFolderInput, DeleteFolderInput, FolderRefInput, FolderWorkspaceInput,
        SetFolderOrderInput, UpdateFolderInput,
    },
    note::{
        crud::{CreateNoteInput, DeleteNoteInput, UpdateNoteInput},
        edit::EditNoteInput,
        get::GetNoteInput,
        history::HistoryInput,
        image::UploadNoteImageInput,
        list::ListNotesInput,
        trash::{ListTrashInput, RestoreNoteInput},
    },
    reply,
    sync::{
        check::CheckNoteSyncInput, pull::PullNoteInput, push::PushNoteInput,
        snapshot::SaveRemoteSnapshotInput,
    },
};

/// MCP server whose handlers share one configured `HackMD` client.
#[derive(Debug, Clone)]
pub(crate) struct HackmdServer {
    client: Arc<HackmdClient>,
}

impl HackmdServer {
    pub(crate) fn new(client: Arc<HackmdClient>) -> Self {
        Self { client }
    }
}

pub(crate) async fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
    let client = Arc::new(HackmdClient::new(Config::from_env()?)?);
    if !client.has_api_token() {
        tracing::warn!("HACKMD_API_TOKEN is not set; API tools will return a configuration error");
    }

    HackmdServer::new(client)
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[tool_router]
impl HackmdServer {
    #[tool(
        name = "hackmd_get_me",
        description = "Get the authenticated HackMD profile, including userPath for resolving personal note URLs.",
        annotations(
            title = "Get HackMD Profile",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_me(
        &self,
        Parameters(EmptyInput {}): Parameters<EmptyInput>,
    ) -> rmcp::model::CallToolResult {
        match self.client.get_me().await {
            Ok(profile) => profile_result(&profile),
            Err(error) => error.into(),
        }
    }

    #[tool(
        name = "hackmd_list_teams",
        description = "List teams available to the authenticated HackMD account. Use each returned path as workspace.team_path.",
        annotations(
            title = "List HackMD Teams",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_teams(
        &self,
        Parameters(EmptyInput {}): Parameters<EmptyInput>,
    ) -> rmcp::model::CallToolResult {
        match self.client.list_teams().await {
            Ok(teams) => teams_result(&teams),
            Err(error) => error.into(),
        }
    }

    #[tool(
        name = "hackmd_list_notes",
        description = "List personal or team HackMD notes with local metadata filtering, deterministic sorting, and pagination. This does not search note content.",
        annotations(
            title = "List HackMD Notes",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_notes(
        &self,
        Parameters(input): Parameters<ListNotesInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::list::list_notes(&self.client, input).await {
            Ok(output) => reply::success(
                format!(
                    "Found {} matching HackMD note(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                serde_json::to_value(output).expect("list-notes output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_get_note",
        description = "Get one HackMD note with full content, normalized metadata, folder_ids, and the exact patch_path for safe edits.",
        annotations(
            title = "Get HackMD Note",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_note(
        &self,
        Parameters(input): Parameters<GetNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::get::get_note(&self.client, input).await {
            Ok(Ok(note)) => reply::success(
                format!("Fetched HackMD note {}", note.id),
                serde_json::json!({"note": note}),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_create_note",
        description = "Create a HackMD note in a personal or team workspace. Folder placement is read back after POST; a compatibility PATCH runs only if the API dropped parentFolderId.",
        annotations(
            title = "Create HackMD Note",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn create_note(
        &self,
        Parameters(input): Parameters<CreateNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::crud::create_note(&self.client, input).await {
            Ok(output) => reply::success(
                format!("Created HackMD note {}", output.note.id),
                serde_json::json!({"result": output}),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_update_note",
        description = "Fallback note update for metadata or an explicit full content replacement. Prefer hackmd_edit_note for normal body edits because content here overwrites the complete unversioned body.",
        annotations(
            title = "Update HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn update_note(
        &self,
        Parameters(input): Parameters<UpdateNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::crud::update_note(&self.client, input).await {
            Ok(Ok(output)) => reply::success(
                format!("HackMD accepted the update for note {}", output.note_id),
                serde_json::json!({"result": output}),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_delete_note",
        description = "Delete a HackMD note. Personal deletion moves it to recoverable trash; team restore is not exposed. This remains destructive.",
        annotations(
            title = "Delete HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn delete_note(
        &self,
        Parameters(input): Parameters<DeleteNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::crud::delete_note(&self.client, input).await {
            Ok(Ok(output)) => reply::success(
                format!("Deleted HackMD note {}", output.note_id),
                serde_json::json!({"result": output}),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_list_trash",
        description = "List trashed personal HackMD notes with slim metadata and client-side pagination.",
        annotations(
            title = "List Trashed HackMD Notes",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_trash(
        &self,
        Parameters(input): Parameters<ListTrashInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::trash::list_trash(&self.client, input).await {
            Ok(output) => reply::success(
                format!(
                    "Found {} trashed HackMD note(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                serde_json::to_value(output).expect("trash-list output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_restore_note",
        description = "Restore a personal HackMD note from trash by internal note ID.",
        annotations(
            title = "Restore Trashed HackMD Note",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn restore_note(
        &self,
        Parameters(input): Parameters<RestoreNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::trash::restore_note(&self.client, input).await {
            Ok(output) => reply::success(
                format!("Restored HackMD note {}", output.note_id),
                serde_json::json!({"result": output}),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_edit_note",
        description = "Default tool for normal HackMD body edits. Applies one strict Codex patch to the current content only when every hunk context is unique; prefer this over hackmd_update_note for body changes.",
        annotations(
            title = "Edit HackMD Note Safely",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn edit_note(
        &self,
        Parameters(input): Parameters<EditNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::edit::edit_note(&self.client, input).await {
            Ok(Ok(output)) => {
                let summary = if output.changed {
                    format!("Edited HackMD note {}", output.note_id)
                } else {
                    format!("HackMD note {} is unchanged", output.note_id)
                };
                reply::success(summary, serde_json::json!({"result": output}))
            }
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_get_history",
        description = "Get recently viewed HackMD notes in API history order with slim metadata and client-side pagination.",
        annotations(
            title = "Get HackMD Browse History",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_history(
        &self,
        Parameters(input): Parameters<HistoryInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::history::get_history(&self.client, input).await {
            Ok(output) => reply::success(
                format!(
                    "Found {} HackMD history item(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                serde_json::to_value(output).expect("history output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

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
    async fn list_folders(
        &self,
        Parameters(input): Parameters<FolderWorkspaceInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::list_folders(&self.client, input).await {
            Ok(output) => reply::success(
                format!(
                    "Found {} HackMD folder(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                serde_json::to_value(output).expect("folder-list output should serialize"),
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
    async fn get_folder(
        &self,
        Parameters(input): Parameters<FolderRefInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::get_folder(&self.client, input).await {
            Ok(output) => reply::success(
                format!("Fetched HackMD folder {}", output.folder.id),
                serde_json::to_value(output).expect("folder output should serialize"),
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
    async fn create_folder(
        &self,
        Parameters(input): Parameters<CreateFolderInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::create_folder(&self.client, input).await {
            Ok(output) => reply::success(
                format!("Created HackMD folder {}", output.folder.id),
                serde_json::to_value(output).expect("folder output should serialize"),
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
    async fn update_folder(
        &self,
        Parameters(input): Parameters<UpdateFolderInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::update_folder(&self.client, input).await {
            Ok(output) => reply::success(
                format!("Updated HackMD folder {}", output.folder.id),
                serde_json::to_value(output).expect("folder output should serialize"),
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
    async fn delete_folder(
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
                reply::success(
                    summary,
                    serde_json::to_value(output).expect("folder-delete output should serialize"),
                )
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
    async fn set_folder_order(
        &self,
        Parameters(input): Parameters<SetFolderOrderInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::folders::set_folder_order(&self.client, input).await {
            Ok(output) => reply::success(
                format!("Set HackMD folder order for {}", output.parent),
                serde_json::to_value(output).expect("folder-order output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_upload_note_image",
        description = "Upload a local image to a personal-workspace note and return only its HackMD CDN link. Files above 5 MiB require confirmation; files above 10 MiB are refused.",
        annotations(
            title = "Upload HackMD Note Image",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn upload_note_image(
        &self,
        Parameters(input): Parameters<UploadNoteImageInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::image::upload_note_image(&self.client, input).await {
            Ok(Ok(output)) => reply::success(
                "Uploaded HackMD note image",
                serde_json::to_value(output).expect("image-upload output should serialize"),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_pull_note",
        description = "Pull one HackMD note's exact Markdown body to an absolute local path and atomically record a private sync baseline. Existing files require overwrite_local: true.",
        annotations(
            title = "Pull HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn pull_note(
        &self,
        Parameters(input): Parameters<PullNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::pull::pull_note(&self.client, input).await {
            Ok(Ok(output)) => reply::success(
                format!("Pulled HackMD note {}", output.note_id),
                serde_json::to_value(output).expect("pull output should serialize"),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_push_note",
        description = "Push a tracked local Markdown file with safe baseline comparison by default. strategy: overwrite requires confirm: true and replaces unversioned remote content.",
        annotations(
            title = "Push HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn push_note(
        &self,
        Parameters(input): Parameters<PushNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::push::push_note(&self.client, input).await {
            Ok(Ok(output)) => reply::success(
                "Evaluated tracked HackMD note push",
                serde_json::to_value(output).expect("push output should serialize"),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_check_note_sync",
        description = "Read local, private baseline, and remote note state without writing, returning in_sync, remote_changed, local_changed, or conflict plus SHA-256 hashes.",
        annotations(
            title = "Check HackMD Note Sync",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn check_note_sync(
        &self,
        Parameters(input): Parameters<CheckNoteSyncInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::check::check_note_sync(&self.client, input).await {
            Ok(output) => reply::success(
                "Checked tracked HackMD note sync state",
                serde_json::to_value(output).expect("sync-check output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_save_remote_snapshot",
        description = "Atomically save the tracked note's current remote body as sibling *.remote.md without changing the working Markdown file. Existing snapshots require explicit overwrite.",
        annotations(
            title = "Save HackMD Remote Snapshot",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn save_remote_snapshot(
        &self,
        Parameters(input): Parameters<SaveRemoteSnapshotInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::snapshot::save_remote_snapshot(&self.client, input).await {
            Ok(output) => reply::success(
                "Saved HackMD remote snapshot",
                serde_json::to_value(output).expect("snapshot output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "the handler bodies are generated by rmcp's tool_handler macro"
)]
#[rmcp::tool_handler(router = Self::tool_router())]
impl rmcp::ServerHandler for HackmdServer {
    /// Wraps every dispatch in one span and one retry scope, so each tool call
    /// carries a request ID through its logs and reports in `_meta` how much
    /// retrying it took.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let request_id = crate::observability::next_request_id();
        let tool_name = request.name.to_string();
        let span = tracing::info_span!(
            "mcp_tool_call",
            request_id = %request_id,
            tool = %tool_name
        );
        let tool_context =
            rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        async move {
            tracing::info!("tool call started");
            let result = crate::retry::scope_call(Self::tool_router().call(tool_context)).await;
            match &result {
                Ok(rmcp::model::CallToolResponse::Complete(response)) => tracing::info!(
                    is_error = response.is_error.unwrap_or(false),
                    "tool call completed"
                ),
                Ok(_) => tracing::info!("tool call yielded an intermediate response"),
                Err(error) => tracing::warn!(code = ?error.code, "tool call rejected"),
            }
            result
        }
        .instrument(span)
        .await
    }
}

fn profile_result(profile: &ProfileResponse) -> rmcp::model::CallToolResult {
    let summary = format!(
        "Authenticated as {} (userPath: {})",
        profile.name, profile.user_path
    );
    reply::success(summary, serde_json::json!({"profile": profile}))
}

fn teams_result(teams: &[TeamResponse]) -> rmcp::model::CallToolResult {
    let summary = format!("Found {} HackMD team(s)", teams.len());
    reply::success(summary, serde_json::json!({"teams": teams}))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[cfg(test)]
mod tests {
    use super::{EmptyInput, HackmdServer, profile_result, teams_result};
    use crate::client::HackmdClient;
    use crate::config::Config;
    use rmcp::{ServerHandler, ServiceExt, model::CallToolRequestParams};
    use serde_json::json;
    use std::sync::Arc;

    const EXPECTED_ANNOTATIONS: [(&str, bool, bool, bool); 22] = [
        ("hackmd_get_me", true, false, true),
        ("hackmd_list_teams", true, false, true),
        ("hackmd_list_notes", true, false, true),
        ("hackmd_get_note", true, false, true),
        ("hackmd_create_note", false, false, false),
        ("hackmd_update_note", false, true, true),
        ("hackmd_delete_note", false, true, true),
        ("hackmd_list_trash", true, false, true),
        ("hackmd_restore_note", false, false, true),
        ("hackmd_edit_note", false, false, false),
        ("hackmd_get_history", true, false, true),
        ("hackmd_list_folders", true, false, true),
        ("hackmd_get_folder", true, false, true),
        ("hackmd_create_folder", false, false, false),
        ("hackmd_update_folder", false, false, true),
        ("hackmd_delete_folder", false, true, true),
        ("hackmd_set_folder_order", false, false, true),
        ("hackmd_upload_note_image", false, false, false),
        ("hackmd_pull_note", false, true, false),
        ("hackmd_push_note", false, true, true),
        ("hackmd_check_note_sync", true, false, true),
        ("hackmd_save_remote_snapshot", false, true, false),
    ];

    async fn protocol_client(
        config: Config,
    ) -> (
        rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tokio::task::JoinHandle<()>,
    ) {
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server = HackmdServer::new(Arc::new(
            HackmdClient::new(config).expect("protocol-test client should build"),
        ));
        let server_task = tokio::spawn(async move {
            server
                .serve(server_transport)
                .await
                .expect("protocol-test server should start")
                .waiting()
                .await
                .expect("protocol-test server task should join");
        });
        let client = ().serve(client_transport).await.expect("RMCP client should connect");
        (client, server_task)
    }

    fn call(name: &'static str, arguments: serde_json::Value) -> CallToolRequestParams {
        let serde_json::Value::Object(arguments) = arguments else {
            panic!("tool arguments should be an object");
        };
        CallToolRequestParams::new(name).with_arguments(arguments)
    }

    async fn stop_protocol(
        client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
        server_task: tokio::task::JoinHandle<()>,
    ) {
        client.cancel().await.expect("RMCP client should cancel");
        server_task.await.expect("protocol-test server should stop");
    }

    #[test]
    fn server_owns_the_shared_client() {
        let client = Arc::new(
            HackmdClient::new(Config::for_tests()).expect("test client should be constructed"),
        );
        let server = HackmdServer::new(Arc::clone(&client));

        assert_eq!(Arc::strong_count(&client), 2);
        drop(server);
        assert_eq!(Arc::strong_count(&client), 1);
    }

    #[test]
    fn tools_have_generated_schemas_and_exact_annotations() {
        let tools = HackmdServer::tool_router().list_all();

        assert_eq!(tools.len(), 22);
        for (name, read_only, destructive, idempotent) in EXPECTED_ANNOTATIONS {
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("tool {name} should exist"));
            let annotations = tool
                .annotations
                .as_ref()
                .expect("annotations should be generated");
            assert_eq!(annotations.read_only_hint, Some(read_only), "{name}");
            assert_eq!(annotations.destructive_hint, Some(destructive), "{name}");
            assert_eq!(annotations.idempotent_hint, Some(idempotent), "{name}");
            assert_eq!(annotations.open_world_hint, Some(true));
        }
        for name in ["hackmd_get_me", "hackmd_list_teams"] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .expect("discovery tool should exist");
            assert_eq!(tool.input_schema["type"], "object");
            assert_eq!(tool.input_schema["additionalProperties"], false);
        }

        let list = tools
            .iter()
            .find(|tool| tool.name == "hackmd_list_notes")
            .expect("list-notes tool should exist");
        let properties = &list.input_schema["properties"];
        assert_eq!(list.input_schema["additionalProperties"], false);
        assert_eq!(properties["limit"]["default"], 20);
        assert_eq!(properties["limit"]["minimum"], 1);
        assert_eq!(properties["limit"]["maximum"], 100);
        assert_eq!(properties["offset"]["default"], 0);
        assert_eq!(properties["sort"]["default"], "last_changed_desc");
        assert!(properties.get("folder_id").is_none());

        let trash = tools
            .iter()
            .find(|tool| tool.name == "hackmd_list_trash")
            .expect("trash list should exist");
        assert_eq!(trash.input_schema["properties"]["limit"]["default"], 20);
        assert_eq!(trash.input_schema["properties"]["limit"]["maximum"], 100);
        let restore = tools
            .iter()
            .find(|tool| tool.name == "hackmd_restore_note")
            .expect("restore should exist");
        assert_eq!(restore.input_schema["required"], json!(["note_id"]));

        let get_note = tools
            .iter()
            .find(|tool| tool.name == "hackmd_get_note")
            .expect("get-note tool should exist");
        assert_eq!(get_note.input_schema["additionalProperties"], false);
        assert!(
            get_note.input_schema["properties"]
                .get("note_ref")
                .is_some()
        );
        assert_eq!(get_note.input_schema["required"], json!(["note_ref"]));

        for name in [
            "hackmd_create_note",
            "hackmd_update_note",
            "hackmd_delete_note",
            "hackmd_edit_note",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("tool {name} should exist"));
            assert_eq!(tool.input_schema["additionalProperties"], false, "{name}");
        }
        let create = tools
            .iter()
            .find(|tool| tool.name == "hackmd_create_note")
            .expect("create tool should exist");
        for field in [
            "comment_permission",
            "suggest_edit_permission",
            "parent_folder_id",
        ] {
            assert!(create.input_schema["properties"].get(field).is_some());
        }
        let update = tools
            .iter()
            .find(|tool| tool.name == "hackmd_update_note")
            .expect("update tool should exist");
        assert_eq!(update.input_schema["required"], json!(["note_ref"]));
        for field in ["comment_permission", "suggest_edit_permission"] {
            assert!(update.input_schema["properties"].get(field).is_some());
        }
        let edit = tools
            .iter()
            .find(|tool| tool.name == "hackmd_edit_note")
            .expect("edit tool should exist");
        assert_eq!(edit.input_schema["required"], json!(["note_ref", "patch"]));
    }

    #[tokio::test]
    async fn tool_call_reports_actionable_missing_token_error() {
        let server = HackmdServer::new(Arc::new(
            HackmdClient::new(Config::for_tests()).expect("test client should be constructed"),
        ));
        let result = server
            .get_me(rmcp::handler::server::wrapper::Parameters(EmptyInput {}))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("error should be text")
                .text,
            "GET /v1/me: HACKMD_API_TOKEN is not configured; set it in the server environment and restart the MCP server"
        );
    }

    #[tokio::test]
    async fn retried_success_and_final_tool_error_include_bounded_metadata() {
        const RETRY_AFTER: &[(&str, &str)] = &[("Retry-After", "0")];
        let retry = crate::config::RetryConfig {
            max_retries: 1,
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(2),
        };
        let success_fixture = crate::fixture::SequenceServer::spawn_with_headers([
            (429, r#"{"error":"rate limited"}"#, RETRY_AFTER),
            (
                200,
                r#"{"id":"u","name":"User","email":"u@example.com","userPath":"user"}"#,
                &[],
            ),
        ]);
        let (client, server_task) = protocol_client(Config::for_loopback_test_with_retry(
            &success_fixture.api_url,
            "fixture-token",
            retry,
        ))
        .await;
        let result = client
            .call_tool(call("hackmd_get_me", json!({})))
            .await
            .expect("retried tool should succeed");
        assert_eq!(result.is_error, Some(false));
        let retry_meta = &result.meta.expect("retried result should have metadata").0["retry"];
        assert_eq!(retry_meta["attempts"], 2);
        assert_eq!(retry_meta["was_rate_limited"], true);
        assert!(
            retry_meta["total_waited_seconds"]
                .as_f64()
                .expect("wait should be numeric")
                <= 0.002
        );
        stop_protocol(client, server_task).await;
        success_fixture.finish();

        let error_fixture = crate::fixture::SequenceServer::spawn([
            (500, r#"{"error":"transient"}"#),
            (500, r#"{"error":"still failing"}"#),
        ]);
        let (client, server_task) = protocol_client(Config::for_loopback_test_with_retry(
            &error_fixture.api_url,
            "fixture-token",
            retry,
        ))
        .await;
        let result = client
            .call_tool(call("hackmd_get_me", json!({})))
            .await
            .expect("tool error should remain a protocol success");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.meta.expect("final error should have metadata").0["retry"]["attempts"],
            2
        );
        stop_protocol(client, server_task).await;
        error_fixture.finish();
    }

    #[tokio::test]
    async fn update_tool_explains_create_only_permissions() {
        let server = HackmdServer::new(Arc::new(
            HackmdClient::new(Config::for_tests()).expect("test client should be constructed"),
        ));
        let input = serde_json::from_value(json!({
            "note_ref": "id",
            "suggest_edit_permission": "owners"
        }))
        .expect("input should deserialize");
        let result = server
            .update_note(rmcp::handler::server::wrapper::Parameters(input))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("error should be text")
                .text,
            "comment_permission and suggest_edit_permission are create-only; HackMD PATCH does not support changing them"
        );
    }

    #[tokio::test]
    async fn edit_conflict_is_a_tool_error_and_never_patches() {
        let fixture = crate::fixture::SequenceServer::spawn([(
            200,
            r#"{"id":"note-id","title":"Title","content":"old"}"#,
        )]);
        let client = fixture.client();
        let server = HackmdServer::new(Arc::new(client));
        let input = serde_json::from_value(json!({
            "note_ref": "note-id",
            "patch": "*** Begin Patch\n*** Update File: notes/other.md\n@@\n-old\n+new\n*** End Patch"
        }))
        .expect("edit input should deserialize");
        let result = server
            .edit_note(rmcp::handler::server::wrapper::Parameters(input))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("error should be text")
                .text,
            "patch targets notes/other.md, expected notes/note-id.md"
        );
        let requests = fixture.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /v1/notes/note-id HTTP/1.1\r\n"));
    }

    #[test]
    fn discovery_results_preserve_resolution_paths() {
        let profile = serde_json::from_value(json!({
            "id": "user-id",
            "name": "Alice",
            "email": "alice@example.test",
            "userPath": "alice",
            "photo": null,
            "teams": []
        }))
        .expect("profile fixture should deserialize");
        let profile_result = profile_result(&profile);
        assert_eq!(
            profile_result.structured_content,
            Some(json!({
                "profile": {
                    "id": "user-id",
                    "name": "Alice",
                    "email": "alice@example.test",
                    "userPath": "alice",
                    "photo": null,
                    "teams": []
                }
            }))
        );

        let teams: Vec<crate::dto::TeamResponse> = serde_json::from_value(json!([{
            "id": "team-id",
            "name": "Engineering",
            "path": "engineering",
            "description": null,
            "hardLimit": 100,
            "visibility": "private"
        }]))
        .expect("team fixture should deserialize");
        let teams_result = teams_result(&teams);
        assert_eq!(
            teams_result
                .structured_content
                .as_ref()
                .expect("structured teams")["teams"][0]["path"],
            "engineering"
        );
    }

    #[test]
    fn server_handler_enables_tools_only() {
        let client =
            HackmdClient::new(Config::for_tests()).expect("test client should be constructed");
        let info = HackmdServer::new(Arc::new(client)).get_info();

        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.prompts.is_none());
        assert!(info.capabilities.resources.is_none());
    }

    #[tokio::test]
    async fn in_process_tools_list_exposes_generated_contract() {
        let (client, server_task) = protocol_client(Config::for_tests()).await;
        let listed = client
            .list_tools(None)
            .await
            .expect("tools/list should succeed");
        assert_eq!(listed.tools.len(), 22);
        for (name, read_only, destructive, idempotent) in EXPECTED_ANNOTATIONS {
            let tool = listed
                .tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("tools/list omitted {name}"));
            let annotations = tool.annotations.as_ref().expect("annotations should exist");
            assert_eq!(annotations.read_only_hint, Some(read_only), "{name}");
            assert_eq!(annotations.destructive_hint, Some(destructive), "{name}");
            assert_eq!(annotations.idempotent_hint, Some(idempotent), "{name}");
        }
        let edit = listed
            .tools
            .iter()
            .find(|tool| tool.name == "hackmd_edit_note")
            .expect("edit tool should be listed");
        assert_eq!(edit.input_schema["required"], json!(["note_ref", "patch"]));
        let annotations = edit.annotations.as_ref().expect("annotations should exist");
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert_eq!(annotations.idempotent_hint, Some(false));
        let list = listed
            .tools
            .iter()
            .find(|tool| tool.name == "hackmd_list_notes")
            .expect("list tool should be listed");
        assert_eq!(list.input_schema["properties"]["limit"]["default"], 20);
        assert_eq!(list.input_schema["properties"]["limit"]["maximum"], 100);
        let update = listed
            .tools
            .iter()
            .find(|tool| tool.name == "hackmd_update_note")
            .expect("update tool should be listed");
        assert_eq!(update.input_schema["required"], json!(["note_ref"]));
        assert!(
            update.input_schema["properties"]
                .get("comment_permission")
                .is_some()
        );
        let history = listed
            .tools
            .iter()
            .find(|tool| tool.name == "hackmd_get_history")
            .expect("history tool should be listed");
        assert_eq!(history.input_schema["properties"]["limit"]["default"], 20);
        assert_eq!(history.input_schema["properties"]["limit"]["maximum"], 100);
        let pull = listed
            .tools
            .iter()
            .find(|tool| tool.name == "hackmd_pull_note")
            .expect("pull tool should be listed");
        assert_eq!(pull.input_schema["additionalProperties"], false);
        assert_eq!(
            pull.input_schema["required"],
            json!(["note_ref", "local_path"])
        );
        assert_eq!(
            pull.input_schema["properties"]["overwrite_local"]["default"],
            false
        );
        assert_eq!(
            pull.input_schema["properties"]["create_parent_dirs"]["default"],
            false
        );
        let push = listed
            .tools
            .iter()
            .find(|tool| tool.name == "hackmd_push_note")
            .expect("push tool should be listed");
        assert_eq!(push.input_schema["additionalProperties"], false);
        assert_eq!(
            push.input_schema["required"],
            json!(["note_ref", "local_path"])
        );
        assert_eq!(
            push.input_schema["properties"]["strategy"]["default"],
            "safe"
        );
        assert_eq!(push.input_schema["properties"]["confirm"]["default"], false);
        let folders = listed
            .tools
            .iter()
            .find(|tool| tool.name == "hackmd_list_folders")
            .expect("folder list should be listed");
        assert_eq!(folders.input_schema["properties"]["limit"]["default"], 20);
        assert_eq!(folders.input_schema["properties"]["limit"]["maximum"], 100);
        assert_eq!(folders.input_schema["properties"]["offset"]["default"], 0);
        stop_protocol(client, server_task).await;
    }

    #[tokio::test]
    async fn in_process_list_call_covers_team_route_search_and_pagination() {
        let fixture = crate::fixture::SequenceServer::spawn([(
            200,
            r#"[
                {"id":"a","title":"Roadmap A","description":"Rust work","tags":["rust"],"lastChangedAt":3},
                {"id":"b","title":"Roadmap B","description":"Rust work","tags":["RUST"],"lastChangedAt":2},
                {"id":"c","title":"Other","description":"Rust work","tags":["rust"],"lastChangedAt":1}
            ]"#,
        )]);
        let (client, server_task) = protocol_client(Config::for_loopback_test(
            &fixture.api_url,
            Some("fixture-token"),
        ))
        .await;
        let result = client
            .call_tool(call(
                "hackmd_list_notes",
                json!({
                    "workspace": {"kind": "team", "team_path": "core/team"},
                    "query": "ROADMAP",
                    "tags": ["rust"],
                    "limit": 1,
                    "offset": 1
                }),
            ))
            .await
            .expect("tools/call should succeed");
        assert_eq!(result.is_error, Some(false));
        let structured = result
            .structured_content
            .expect("structured result should exist");
        assert_eq!(structured["total"], 2);
        assert_eq!(structured["count"], 1);
        assert_eq!(structured["offset"], 1);
        assert_eq!(structured["has_more"], false);
        assert_eq!(structured["notes"][0]["id"], "b");
        stop_protocol(client, server_task).await;
        let requests = fixture.finish();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET /v1/teams/core%2Fteam/notes HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn in_process_edit_calls_cover_no_op_and_conflict_without_patch() {
        let fixture = crate::fixture::SequenceServer::spawn([
            (200, r#"{"id":"same","title":"Same","content":"same"}"#),
            (
                200,
                r#"{"id":"conflict","title":"Conflict","content":"actual"}"#,
            ),
        ]);
        let (client, server_task) = protocol_client(Config::for_loopback_test(
            &fixture.api_url,
            Some("fixture-token"),
        ))
        .await;
        let no_op = client
            .call_tool(call(
                "hackmd_edit_note",
                json!({
                    "note_ref": "same",
                    "patch": "*** Begin Patch\n*** Update File: notes/same.md\n@@\n same\n*** End Patch"
                }),
            ))
            .await
            .expect("no-op tool call should succeed");
        assert_eq!(no_op.is_error, Some(false));
        assert_eq!(
            no_op.structured_content.expect("no-op result")["result"]["changed"],
            false
        );

        let conflict = client
            .call_tool(call(
                "hackmd_edit_note",
                json!({
                    "note_ref": "conflict",
                    "patch": "*** Begin Patch\n*** Update File: notes/conflict.md\n@@\n-missing\n+new\n*** End Patch"
                }),
            ))
            .await
            .expect("conflict tool call should return a tool result");
        assert_eq!(conflict.is_error, Some(true));
        assert_eq!(
            conflict.content[0]
                .as_text()
                .expect("conflict should be text")
                .text,
            "patch hunk context was not found"
        );
        stop_protocol(client, server_task).await;
        let requests = fixture.finish();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with("GET /v1/notes/same HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("GET /v1/notes/conflict HTTP/1.1\r\n"));
    }
}
