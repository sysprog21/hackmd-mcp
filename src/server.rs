use std::sync::Arc;

use rmcp::{ServiceExt, tool_router};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool};
use serde::Deserialize;

use crate::client::HackmdClient;
use crate::config::Config;
use crate::crud::{CreateNoteInput, DeleteNoteInput, UpdateNoteInput};
use crate::dto::{ProfileResponse, TeamResponse};
use crate::edit_note::EditNoteInput;
use crate::get_note::GetNoteInput;
use crate::list_notes::ListNotesInput;
use crate::tool_result;

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

#[tool_router(server_handler)]
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
        match crate::list_notes::list_notes(&self.client, input).await {
            Ok(output) => tool_result::success(
                format!(
                    "Found {} matching HackMD note(s); returned {}",
                    output.total, output.count
                ),
                serde_json::to_value(output).expect("list-notes output should serialize"),
            ),
            Err(error) => tool_result::error(error.to_string()),
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
        match crate::get_note::get_note(&self.client, input).await {
            Ok(Ok(note)) => tool_result::success(
                format!("Fetched HackMD note {}", note.id),
                serde_json::json!({"note": note}),
            ),
            Ok(Err(resolution)) => tool_result::success(
                "The note reference did not resolve uniquely",
                serde_json::json!({"resolution": resolution}),
            ),
            Err(error) => tool_result::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_create_note",
        description = "Create a HackMD note in a personal or team workspace. Folder placement is completed with a follow-up PATCH because HackMD ignores parentFolderId during creation.",
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
        match crate::crud::create_note(&self.client, input).await {
            Ok(output) => tool_result::success(
                format!("Created HackMD note {}", output.note.id),
                serde_json::json!({"result": output}),
            ),
            Err(error) => tool_result::error(error.to_string()),
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
        match crate::crud::update_note(&self.client, input).await {
            Ok(Ok(output)) => tool_result::success(
                format!("HackMD accepted the update for note {}", output.note_id),
                serde_json::json!({"result": output}),
            ),
            Ok(Err(resolution)) => tool_result::success(
                "The note reference did not resolve uniquely",
                serde_json::json!({"resolution": resolution}),
            ),
            Err(error) => tool_result::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_delete_note",
        description = "Delete a HackMD note from a personal or team workspace. This is destructive and may move the note to trash depending on HackMD workspace behavior.",
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
        match crate::crud::delete_note(&self.client, input).await {
            Ok(Ok(output)) => tool_result::success(
                format!("Deleted HackMD note {}", output.note_id),
                serde_json::json!({"result": output}),
            ),
            Ok(Err(resolution)) => tool_result::success(
                "The note reference did not resolve uniquely",
                serde_json::json!({"resolution": resolution}),
            ),
            Err(error) => tool_result::error(error.to_string()),
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
        match crate::edit_note::edit_note(&self.client, input).await {
            Ok(Ok(output)) => {
                let summary = if output.changed {
                    format!("Edited HackMD note {}", output.note_id)
                } else {
                    format!("HackMD note {} is unchanged", output.note_id)
                };
                tool_result::success(summary, serde_json::json!({"result": output}))
            }
            Ok(Err(resolution)) => tool_result::success(
                "The note reference did not resolve uniquely",
                serde_json::json!({"resolution": resolution}),
            ),
            Err(error) => tool_result::error(error.to_string()),
        }
    }
}

fn profile_result(profile: &ProfileResponse) -> rmcp::model::CallToolResult {
    let summary = format!(
        "Authenticated as {} (userPath: {})",
        profile.name, profile.user_path
    );
    tool_result::success(summary, serde_json::json!({"profile": profile}))
}

fn teams_result(teams: &[TeamResponse]) -> rmcp::model::CallToolResult {
    let summary = format!("Found {} HackMD team(s)", teams.len());
    tool_result::success(summary, serde_json::json!({"teams": teams}))
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[allow(dead_code, reason = "constructed by RMCP's generated schema decoder")]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[cfg(test)]
mod tests {
    use super::{EmptyInput, HackmdServer, profile_result, teams_result};
    use crate::client::HackmdClient;
    use crate::config::Config;
    use rmcp::ServerHandler;
    use serde_json::json;
    use std::sync::Arc;

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

        assert_eq!(tools.len(), 8);
        let expected = [
            ("hackmd_get_me", true, false, true),
            ("hackmd_list_teams", true, false, true),
            ("hackmd_list_notes", true, false, true),
            ("hackmd_get_note", true, false, true),
            ("hackmd_create_note", false, false, false),
            ("hackmd_update_note", false, true, true),
            ("hackmd_delete_note", false, true, true),
            ("hackmd_edit_note", false, false, false),
        ];
        for (name, read_only, destructive, idempotent) in expected {
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
        let fixture = crate::test_support::SequenceServer::spawn([(
            200,
            r#"{"id":"note-id","title":"Title","content":"old"}"#,
        )]);
        let client = HackmdClient::new(Config::for_loopback_test(
            &fixture.api_url,
            Some("fixture-token"),
        ))
        .expect("fixture client should be constructed");
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
}
