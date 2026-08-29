use std::sync::Arc;

use rmcp::{ServiceExt, tool_router};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool};
use serde::Deserialize;

use crate::client::HackmdClient;
use crate::config::Config;
use crate::models::Workspace;
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

// Real API tools are introduced by their P1 tasks. This clearly named internal
// probe validates the production router until the first real tool replaces it.
#[tool_router(server_handler)]
impl HackmdServer {
    #[tool(
        name = "_hackmd_schema_probe",
        description = "Internal probe for typed tool schemas"
    )]
    fn schema_probe(
        &self,
        Parameters(SchemaProbeInput {
            workspace,
            note_ref,
        }): Parameters<SchemaProbeInput>,
    ) -> rmcp::model::CallToolResult {
        if !self.client.has_api_token() {
            return tool_result::error(
                "HACKMD_API_TOKEN is not configured; set it in the server environment and restart the MCP server",
            );
        }
        tool_result::success(
            "Schema probe succeeded",
            serde_json::json!({"workspace": workspace, "note_ref": note_ref}),
        )
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[allow(dead_code, reason = "constructed by RMCP's generated schema decoder")]
struct SchemaProbeInput {
    /// Personal account or team workspace; defaults to personal.
    #[serde(default)]
    workspace: Workspace,
    /// Internal note ID or `HackMD` note URL.
    note_ref: String,
}

#[cfg(test)]
mod tests {
    use super::{HackmdServer, SchemaProbeInput};
    use crate::client::HackmdClient;
    use crate::config::Config;
    use crate::models::Workspace;
    use rmcp::ServerHandler;
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
    fn typed_tool_schema_includes_field_documentation() {
        let tools = HackmdServer::tool_router().list_all();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "_hackmd_schema_probe");
        assert_eq!(
            tools[0].input_schema["properties"]["note_ref"]["description"],
            "Internal note ID or `HackMD` note URL."
        );
        assert_eq!(
            tools[0].input_schema["required"],
            serde_json::json!(["note_ref"])
        );
        assert_eq!(
            tools[0].input_schema["properties"]["workspace"]["default"]["kind"],
            "personal"
        );
    }

    #[test]
    fn typed_input_defaults_an_omitted_workspace_to_personal() {
        let input: SchemaProbeInput = serde_json::from_value(serde_json::json!({
            "note_ref": "internal-id"
        }))
        .expect("workspace should be optional");

        assert_eq!(input.workspace, Workspace::Personal);
    }

    #[test]
    fn tool_call_reports_actionable_missing_token_error() {
        let server = HackmdServer::new(Arc::new(
            HackmdClient::new(Config::for_tests()).expect("test client should be constructed"),
        ));
        let result = server.schema_probe(rmcp::handler::server::wrapper::Parameters(
            SchemaProbeInput {
                workspace: Workspace::Personal,
                note_ref: "internal-id".to_owned(),
            },
        ));

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("error should be text")
                .text,
            "HACKMD_API_TOKEN is not configured; set it in the server environment and restart the MCP server"
        );
    }

    #[test]
    fn tool_call_success_contains_text_and_structured_json() {
        let config = Config::for_loopback_test("http://127.0.0.1:1/v1", Some("fixture-token"));
        let server = HackmdServer::new(Arc::new(
            HackmdClient::new(config).expect("test client should be constructed"),
        ));
        let result = server.schema_probe(rmcp::handler::server::wrapper::Parameters(
            SchemaProbeInput {
                workspace: Workspace::Team {
                    team_path: "engineering".to_owned(),
                },
                note_ref: "note-id".to_owned(),
            },
        ));

        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("success should be text")
                .text,
            "Schema probe succeeded"
        );
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({
                "workspace": {"kind": "team", "team_path": "engineering"},
                "note_ref": "note-id"
            }))
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
