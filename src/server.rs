use std::sync::Arc;

use rmcp::{ServiceExt, tool_router};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool};
use serde::Deserialize;

use crate::client::HackmdClient;
use crate::config::Config;
use crate::dto::{ProfileResponse, TeamResponse};
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
    fn discovery_tools_have_generated_closed_empty_schemas_and_annotations() {
        let tools = HackmdServer::tool_router().list_all();

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "hackmd_get_me");
        assert_eq!(tools[1].name, "hackmd_list_teams");
        for tool in tools {
            assert_eq!(tool.input_schema["type"], "object");
            assert_eq!(tool.input_schema["additionalProperties"], false);
            let annotations = tool.annotations.expect("annotations should be generated");
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.destructive_hint, Some(false));
            assert_eq!(annotations.idempotent_hint, Some(true));
            assert_eq!(annotations.open_world_hint, Some(true));
        }
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
