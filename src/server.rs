use std::sync::Arc;

use rmcp::tool_router;
use rmcp::{handler::server::wrapper::Parameters, schemars, tool};
use serde::Deserialize;

use crate::client::HackmdClient;

/// MCP server whose handlers share one configured `HackMD` client.
#[derive(Debug, Clone)]
#[allow(dead_code, reason = "constructed by the following server startup task")]
pub(crate) struct HackmdServer {
    client: Arc<HackmdClient>,
}

impl HackmdServer {
    #[allow(dead_code, reason = "called by the following server startup task")]
    pub(crate) fn new(client: Arc<HackmdClient>) -> Self {
        Self { client }
    }
}

// Real API tools are introduced by their P1 tasks. This internal probe is not
// reachable by users yet because stdio serving is added by the following task;
// the first real tool will replace it before the server becomes functional.
#[tool_router(server_handler)]
impl HackmdServer {
    #[tool(
        name = "_hackmd_schema_probe",
        description = "Internal probe for typed tool schemas"
    )]
    fn schema_probe(
        &self,
        Parameters(SchemaProbeInput { note_ref }): Parameters<SchemaProbeInput>,
    ) -> String {
        format!("{note_ref}:{}", Arc::strong_count(&self.client))
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[allow(dead_code, reason = "constructed by RMCP's generated schema decoder")]
struct SchemaProbeInput {
    /// Internal note ID or `HackMD` note URL.
    note_ref: String,
}

#[cfg(test)]
mod tests {
    use super::HackmdServer;
    use crate::client::HackmdClient;
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
        assert_eq!(tools[0].input_schema["required"][0], "note_ref");
    }

    #[test]
    fn server_handler_enables_tools_only() {
        let info = HackmdServer::new(Arc::new(HackmdClient)).get_info();

        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.prompts.is_none());
        assert!(info.capabilities.resources.is_none());
    }
}
