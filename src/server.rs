use std::sync::Arc;

use rmcp::{ServiceExt, tool_router};
use rmcp::{handler::server::wrapper::Parameters, schemars, tool};
use serde::Deserialize;

use crate::client::HackmdClient;
use crate::models::Workspace;

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
    HackmdServer::new(Arc::new(HackmdClient))
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
    ) -> String {
        format!(
            "{workspace:?}:{note_ref}:{}",
            Arc::strong_count(&self.client)
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
    use crate::models::Workspace;
    use rmcp::ServerHandler;
    use std::sync::Arc;

    #[test]
    fn server_owns_the_shared_client() {
        let client = Arc::new(HackmdClient);
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
    fn server_handler_enables_tools_only() {
        let info = HackmdServer::new(Arc::new(HackmdClient)).get_info();

        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.prompts.is_none());
        assert!(info.capabilities.resources.is_none());
    }
}
