use std::sync::Arc;

use rmcp::ServiceExt;
use tracing::Instrument;

use crate::{client::HackmdClient, config::Config, local::LocalFiles};

/// MCP server whose handlers share one configured `HackMD` client.
#[derive(Debug, Clone)]
pub(crate) struct HackmdServer {
    client: Arc<HackmdClient>,
    /// Local storage and path policy. The client stays purely about HTTP.
    files: Arc<LocalFiles>,
}

mod account;
mod folder;
mod note;
mod sync;

impl HackmdServer {
    pub(crate) fn new(client: Arc<HackmdClient>, files: Arc<LocalFiles>) -> Self {
        Self { client, files }
    }

    /// The 14 tools, assembled from one router per family. Splitting them keeps
    /// each file about a single part of the API; the router the transport sees
    /// is the same either way.
    ///
    /// Assembled once: dispatch happens per tool call, and rebuilding four
    /// routers to merge them every time is work that never changes.
    fn router() -> &'static rmcp::handler::server::router::tool::ToolRouter<Self> {
        static ROUTER: std::sync::OnceLock<
            rmcp::handler::server::router::tool::ToolRouter<HackmdServer>,
        > = std::sync::OnceLock::new();
        ROUTER.get_or_init(|| {
            Self::account_router()
                + Self::note_router()
                + Self::folder_router()
                + Self::sync_router()
        })
    }
}

pub(crate) async fn run_stdio() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let files = Arc::new(LocalFiles::from_config(&config));
    if let Some(root) = config.workspace_root() {
        match files.probe_workspace_root() {
            Ok(()) => {
                tracing::info!(root = %root.display(), "local file tools are confined to this tree");
            }
            Err(error) => tracing::warn!(
                root = %root.display(),
                error = %error,
                "local file tools will refuse every path until the server restarts with this root present"
            ),
        }
    } else {
        tracing::warn!(
            "HACKMD_MCP_WORKSPACE_ROOT is unset: local file tools accept any absolute path; set it to confine them"
        );
    }
    if !config.has_api_token() {
        tracing::warn!("HACKMD_API_TOKEN is not set; API tools will return a configuration error");
    }
    let client = Arc::new(HackmdClient::new(config)?);

    HackmdServer::new(client, files)
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[allow(
    unknown_lints,
    clippy::unused_async_trait_impl,
    reason = "rmcp generates the trait method body and requires its async signature"
)]
#[rmcp::tool_handler(router = Self::router().clone())]
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
            let result = crate::retry::scope_call(Self::router().call(tool_context)).await;
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

#[cfg(test)]
mod tests {
    /// Local access with no configured root: tool tests exercise the tools, not
    /// the path policy, which has its own tests in `local`.
    fn test_files() -> Arc<crate::local::LocalFiles> {
        Arc::new(crate::fixture::scratch_files())
    }

    use super::HackmdServer;
    use crate::account::{EmptyInput, profile_summary};
    use crate::client::HackmdClient;
    use crate::config::Config;
    use crate::fixture::{Scenario, SequenceServer};
    use rmcp::{ServerHandler, ServiceExt, model::CallToolRequestParams};
    use serde_json::json;
    use std::sync::Arc;

    const EXPECTED_ANNOTATIONS: [(&str, bool, bool, bool); 14] = [
        ("hackmd_get_me", true, false, true),
        ("hackmd_list_notes", true, false, true),
        ("hackmd_get_note", true, false, true),
        ("hackmd_create_note", false, false, false),
        ("hackmd_update_note", false, true, false),
        ("hackmd_delete_note", false, true, true),
        ("hackmd_list_folders", true, false, true),
        ("hackmd_create_folder", false, false, false),
        ("hackmd_update_folder", false, false, true),
        ("hackmd_delete_folder", false, true, true),
        ("hackmd_upload_note_image", false, true, false),
        ("hackmd_pull_note", false, true, false),
        ("hackmd_push_note", false, true, true),
        ("hackmd_untrack_note", false, true, true),
    ];

    async fn protocol_client(
        config: Config,
    ) -> (
        rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tokio::task::JoinHandle<()>,
    ) {
        protocol_client_with_files(config, test_files()).await
    }

    async fn protocol_client_with_files(
        config: Config,
        files: Arc<crate::local::LocalFiles>,
    ) -> (
        rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tokio::task::JoinHandle<()>,
    ) {
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server = HackmdServer::new(
            Arc::new(HackmdClient::new(config).expect("protocol-test client should build")),
            files,
        );
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

    fn note_lifecycle_fixture() -> SequenceServer {
        SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"u","name":"User","userPath":"alice"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes",
                200,
                r#"[{"id":"note-id","title":"Note","permalink":"slug"}]"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"old"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"old"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, "")
                .expect_body("the edited Markdown body", |body| {
                    body == r#"{"content":"new"}"#
                }),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"new"}"#,
            ),
            Scenario::new(
                "POST",
                "/v1/notes",
                201,
                r#"{"id":"new-id","title":"Placed"}"#,
            )
            .expect_body("the requested folder placement", |body| {
                body == r#"{"title":"Placed","parentFolderId":"folder-id"}"#
            }),
            Scenario::new(
                "GET",
                "/v1/notes/new-id",
                200,
                r#"{"id":"new-id","title":"Placed","folderPaths":[{"id":"folder-id","name":"Folder"}]}"#,
            ),
            Scenario::new("DELETE", "/v1/notes/new-id", 204, ""),
            Scenario::new(
                "PUT",
                "/v1/trash/new-id/restore",
                200,
                r#"{"restored":true}"#,
            ),
        ])
    }

    fn sync_lifecycle_fixture() -> SequenceServer {
        SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"baseline","lastChangedAt":1}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"baseline","lastChangedAt":1}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"baseline","lastChangedAt":1}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, "")
                .expect_body("the locally edited body", |body| {
                    body == r#"{"content":"local edit"}"#
                }),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"local edit","lastChangedAt":2}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Note","content":"remote edit","lastChangedAt":3}"#,
            ),
        ])
    }

    #[test]
    fn server_owns_the_shared_client() {
        let client = Arc::new(
            HackmdClient::new(Config::for_tests()).expect("test client should be constructed"),
        );
        let server = HackmdServer::new(Arc::clone(&client), test_files());

        assert_eq!(Arc::strong_count(&client), 2);
        drop(server);
        assert_eq!(Arc::strong_count(&client), 1);
    }

    #[test]
    fn tools_have_generated_schemas_and_exact_annotations() {
        let tools = HackmdServer::router().list_all();

        assert_eq!(tools.len(), EXPECTED_ANNOTATIONS.len());
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
            let open_world = name != "hackmd_untrack_note";
            assert_eq!(annotations.open_world_hint, Some(open_world), "{name}");
        }
        let discovery = tools
            .iter()
            .find(|tool| tool.name == "hackmd_get_me")
            .expect("discovery tool should exist");
        assert_eq!(discovery.input_schema["type"], "object");
        assert_eq!(discovery.input_schema["additionalProperties"], false);

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
        assert_eq!(properties["source"]["default"], "workspace");
        assert!(properties.get("workspace").is_none());
        assert_eq!(properties["team_path"]["type"], json!(["string", "null"]));
        assert!(properties.get("folder_id").is_none());

        let upload = tools
            .iter()
            .find(|tool| tool.name == "hackmd_upload_note_image")
            .expect("upload should exist");
        assert!(upload.input_schema["properties"].get("team_path").is_none());
        let delete = tools
            .iter()
            .find(|tool| tool.name == "hackmd_delete_note")
            .expect("delete should exist");
        assert_eq!(delete.input_schema["required"], json!(["note_ref"]));
        assert_eq!(
            delete.input_schema["properties"]["restore"]["default"],
            false
        );

        let get_note = tools
            .iter()
            .find(|tool| tool.name == "hackmd_get_note")
            .expect("get-note tool should exist");
        assert_eq!(get_note.input_schema["additionalProperties"], false);
        for field in ["note_ref", "local_path"] {
            assert!(get_note.input_schema["properties"].get(field).is_some());
        }
        // Either one names what to get, so neither is required on its own.
        assert!(get_note.input_schema.get("required").is_none());

        for name in [
            "hackmd_create_note",
            "hackmd_update_note",
            "hackmd_delete_note",
            "hackmd_update_folder",
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
    }

    #[test]
    fn merged_tools_take_their_folded_parameters() {
        let tools = HackmdServer::router().list_all();
        let update = tools
            .iter()
            .find(|tool| tool.name == "hackmd_update_note")
            .expect("update tool should exist");
        assert_eq!(update.input_schema["required"], json!(["note_ref"]));
        for field in ["comment_permission", "suggest_edit_permission", "patch"] {
            assert!(update.input_schema["properties"].get(field).is_some());
        }
        let folder = tools
            .iter()
            .find(|tool| tool.name == "hackmd_update_folder")
            .expect("folder update should exist");
        assert!(
            folder.input_schema["properties"]
                .get("child_order")
                .is_some()
        );
        assert!(folder.input_schema.get("required").is_none());
    }

    #[tokio::test]
    async fn tool_call_reports_actionable_missing_token_error() {
        let server = HackmdServer::new(
            Arc::new(
                HackmdClient::new(Config::for_tests()).expect("test client should be constructed"),
            ),
            test_files(),
        );
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
        let retry = crate::config::RetryConfig {
            max_retries: 1,
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(2),
        };
        let success_fixture = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/me", 429, r#"{"error":"rate limited"}"#)
                .response_header("Retry-After", "0"),
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"u","name":"User","email":"u@example.com","userPath":"user","teams":[{"id":"t","name":"Team","path":"team","description":null,"hardLimit":null}]}"#,
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

        let error_fixture = SequenceServer::spawn_scenarios([
            Scenario::new("GET", "/v1/me", 500, r#"{"error":"transient"}"#),
            Scenario::new("GET", "/v1/me", 500, r#"{"error":"still failing"}"#),
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
        let meta = result.meta.expect("final error should have metadata").0;
        assert_eq!(meta["retry"]["attempts"], 2);
        // Both entries survive: the retry scope adds to the kind, not over it.
        assert_eq!(meta["error_kind"], "upstream");
        stop_protocol(client, server_task).await;
        error_fixture.finish();
    }

    #[tokio::test]
    async fn asynchronous_readback_reports_bounded_attempt_metadata() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"old"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"old"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"new"}"#,
            ),
        ]);
        let (client, server_task) = protocol_client(Config::for_loopback_test(
            &fixture.api_url,
            Some("fixture-token"),
        ))
        .await;
        let result = client
            .call_tool(call(
                "hackmd_update_note",
                json!({
                    "note_ref": "note-id",
                    "patch": "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch"
                }),
            ))
            .await
            .expect("edit tool call should complete");

        assert_eq!(result.is_error, Some(false));
        let retry = &result.meta.expect("readback should emit metadata").0["retry"];
        assert_eq!(retry["attempts"], 1);
        assert_eq!(retry["readback_attempts"], 2);
        assert!(
            retry["readback_elapsed_seconds"]
                .as_f64()
                .expect("elapsed time should be numeric")
                > 0.0
        );
        stop_protocol(client, server_task).await;
        fixture.finish();

        let failed = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                200,
                r#"{"id":"note-id","title":"Title","content":"old"}"#,
            ),
            Scenario::new("PATCH", "/v1/notes/note-id", 202, ""),
            Scenario::new(
                "GET",
                "/v1/notes/note-id",
                500,
                r#"{"error":"readback failed"}"#,
            ),
        ]);
        let (client, server_task) = protocol_client(Config::for_loopback_test_no_retry(
            &failed.api_url,
            "fixture-token",
        ))
        .await;
        let result = client
            .call_tool(call(
                "hackmd_update_note",
                json!({
                    "note_ref": "note-id",
                    "patch": "*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch"
                }),
            ))
            .await
            .expect("failed readback should remain a tool response");
        assert_eq!(result.is_error, Some(true));
        let meta = result.meta.expect("failed readback should emit metadata").0;
        assert_eq!(meta["retry"]["readback_attempts"], 1);
        // The read-back itself failed with a 500, which is what gets classed.
        assert_eq!(meta["error_kind"], "upstream");
        stop_protocol(client, server_task).await;
        failed.finish();
    }

    #[tokio::test]
    async fn update_tool_explains_create_only_permissions() {
        let server = HackmdServer::new(
            Arc::new(
                HackmdClient::new(Config::for_tests()).expect("test client should be constructed"),
            ),
            test_files(),
        );
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
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/notes/note-id",
            200,
            r#"{"id":"note-id","title":"Title","content":"old"}"#,
        )]);
        let client = fixture.client();
        let server = HackmdServer::new(Arc::new(client), test_files());
        let input = serde_json::from_value(json!({
            "note_ref": "note-id",
            "patch": "*** Begin Patch\n*** Update File: notes/other.md\n@@\n-old\n+new\n*** End Patch"
        }))
        .expect("edit input should deserialize");
        let result = server
            .update_note(rmcp::handler::server::wrapper::Parameters(input))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("error should be text")
                .text,
            "patch targets notes/other.md, expected notes/note-id.md"
        );
        fixture.finish();
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
        let profile_result = crate::reply::respond(
            Ok::<_, crate::client::HackmdError>(profile),
            profile_summary,
        );
        let expected = json!({
            "id": "user-id",
            "name": "Alice",
            "email": "alice@example.test",
            "user_path": "alice",
            "photo": null,
            "teams": []
        });
        assert_eq!(profile_result.structured_content, Some(expected.clone()));
        let text = profile_result.content[1]
            .as_text()
            .expect("JSON text block should exist");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text.text).expect("text should be JSON"),
            expected
        );
    }

    #[test]
    fn server_handler_enables_tools_only() {
        let client =
            HackmdClient::new(Config::for_tests()).expect("test client should be constructed");
        let info = HackmdServer::new(Arc::new(client), test_files()).get_info();

        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.prompts.is_none());
        assert!(info.capabilities.resources.is_none());
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "one contract test intentionally verifies the complete generated tool catalog"
    )]
    async fn in_process_tools_list_exposes_generated_contract() {
        let (client, server_task) = protocol_client(Config::for_tests()).await;
        let listed = client
            .list_tools(None)
            .await
            .expect("tools/list should succeed");
        assert_eq!(listed.tools.len(), EXPECTED_ANNOTATIONS.len());
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
        for field in ["comment_permission", "patch"] {
            assert!(update.input_schema["properties"].get(field).is_some());
        }
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
        assert_eq!(push.input_schema["required"], json!(["local_path"]));
        assert_eq!(
            push.input_schema["properties"]["strategy"]["default"],
            "safe"
        );
        assert_eq!(push.input_schema["properties"]["confirm"]["default"], false);
        assert!(
            serde_json::Value::Object((*list.input_schema).clone())
                .to_string()
                .contains(r#""tracked""#),
            "the tracked source should be advertised"
        );
        // The tracked source pages with the same limits the old tool had.
        assert_eq!(list.input_schema["properties"]["limit"]["default"], 20);
        assert_eq!(list.input_schema["properties"]["limit"]["minimum"], 1);
        assert_eq!(list.input_schema["properties"]["offset"]["default"], 0);
        let untrack = listed
            .tools
            .iter()
            .find(|tool| tool.name == "hackmd_untrack_note")
            .expect("untrack tool should be listed");
        assert_eq!(untrack.input_schema["required"], json!(["note_id"]));
        assert_eq!(
            untrack.input_schema["properties"]["confirm"]["default"],
            false
        );
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
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/teams/core%2Fteam/notes",
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
                    "team_path": "core/team",
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
        // Output names the workspace the way input takes it.
        assert_eq!(structured["notes"][0]["team_path"], "core/team");
        assert!(structured["notes"][0].get("workspace").is_none());
        stop_protocol(client, server_task).await;
        fixture.finish();
    }

    #[tokio::test]
    async fn in_process_edit_calls_cover_no_op_and_conflict_without_patch() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/notes/same",
                200,
                r#"{"id":"same","title":"Same","content":"same"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/notes/conflict",
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
                "hackmd_update_note",
                json!({
                    "note_ref": "same",
                    "patch": "*** Begin Patch\n*** Update File: notes/same.md\n@@\n same\n*** End Patch"
                }),
            ))
            .await
            .expect("no-op tool call should succeed");
        assert_eq!(no_op.is_error, Some(false));
        assert_eq!(
            no_op.structured_content.expect("no-op result")["changed"],
            false
        );

        let conflict = client
            .call_tool(call(
                "hackmd_update_note",
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
        fixture.finish();
    }

    #[tokio::test]
    async fn rmcp_note_lifecycle_covers_resolution_edit_folder_delete_and_restore() {
        let fixture = note_lifecycle_fixture();
        let (client, server_task) = protocol_client(Config::for_loopback_test(
            &fixture.api_url,
            Some("fixture-token"),
        ))
        .await;

        let get = client
            .call_tool(call(
                "hackmd_get_note",
                json!({"note_ref":"https://hackmd.io/@alice/slug"}),
            ))
            .await
            .expect("resolved get should complete");
        assert_eq!(get.is_error, Some(false));
        let get = get.structured_content.expect("get result");
        assert_eq!(get["id"], "note-id");
        assert_eq!(get["mode"], "note");

        let edit = client
            .call_tool(call(
                "hackmd_update_note",
                json!({
                    "note_ref":"note-id",
                    "patch":"*** Begin Patch\n*** Update File: notes/note-id.md\n@@\n-old\n+new\n*** End Patch"
                }),
            ))
            .await
            .expect("edit should complete");
        assert_eq!(edit.is_error, Some(false));
        let edit = edit.structured_content.expect("edit result");
        assert_eq!(edit["changed"], true);
        assert_eq!(edit["mode"], "patched");

        let create = client
            .call_tool(call(
                "hackmd_create_note",
                json!({"title":"Placed","parent_folder_id":"folder-id"}),
            ))
            .await
            .expect("create should complete");
        assert_eq!(create.is_error, Some(false));
        let created = create.structured_content.expect("create result");
        assert_eq!(created["folder_placement_confirmed"], true);
        assert_eq!(created["compatibility_patch_applied"], false);

        let deleted = client
            .call_tool(call("hackmd_delete_note", json!({"note_ref":"new-id"})))
            .await
            .expect("delete should complete");
        assert_eq!(deleted.is_error, Some(false));
        let restored = client
            .call_tool(call(
                "hackmd_delete_note",
                json!({"note_ref":"new-id","restore":true}),
            ))
            .await
            .expect("restore should complete");
        assert_eq!(restored.is_error, Some(false));

        stop_protocol(client, server_task).await;
        fixture.finish();
    }

    #[tokio::test]
    async fn rmcp_sync_lifecycle_covers_pull_check_push_and_conflict() {
        let directory = tempfile::tempdir().expect("workflow directory should create");
        let root = directory.path().join("workspace");
        std::fs::create_dir(&root).expect("workspace root should create");
        let local_path = root.join("note.md");
        let state_dir = directory.path().join("state");
        let fixture = sync_lifecycle_fixture();
        let files = Arc::new(crate::local::LocalFiles::new(state_dir.clone(), Some(root)));
        let (client, server_task) = protocol_client_with_files(
            Config::for_loopback_test(&fixture.api_url, Some("fixture-token")),
            files,
        )
        .await;
        let path = local_path.to_string_lossy().into_owned();

        let pull = client
            .call_tool(call(
                "hackmd_pull_note",
                json!({"note_ref":"note-id","local_path":&path,"create_parent_dirs":true}),
            ))
            .await
            .expect("pull should complete");
        assert_eq!(pull.is_error, Some(false));
        assert_eq!(pull.structured_content.expect("pull result")["bytes"], 8);
        assert_eq!(
            std::fs::read_to_string(&local_path).expect("pulled note should read"),
            "baseline"
        );

        let check = client
            .call_tool(call("hackmd_get_note", json!({"local_path":&path})))
            .await
            .expect("check should complete");
        assert_eq!(check.is_error, Some(false));
        let check = check.structured_content.expect("check result");
        assert_eq!(check["status"], "in_sync");
        assert_eq!(check["mode"], "sync");

        std::fs::write(&local_path, "local edit").expect("local edit should write");
        let push = client
            .call_tool(call(
                "hackmd_push_note",
                json!({"note_ref":"note-id","local_path":&path}),
            ))
            .await
            .expect("push should complete");
        assert_eq!(push.is_error, Some(false));
        assert_eq!(
            push.structured_content.expect("push result")["status"],
            "pushed"
        );

        std::fs::write(&local_path, "second local").expect("second local edit should write");
        let conflict = client
            .call_tool(call(
                "hackmd_push_note",
                json!({"note_ref":"note-id","local_path":&path}),
            ))
            .await
            .expect("conflict check should complete");
        assert_eq!(conflict.is_error, Some(false));
        let conflict = conflict.structured_content.expect("conflict result");
        assert_eq!(conflict["status"], "conflict");
        assert!(conflict["baseline_path"].is_string());
        let snapshot = conflict["snapshot_path"]
            .as_str()
            .expect("conflict should save a snapshot inside the root");
        assert!(snapshot.ends_with("note.remote.md"));
        assert!(conflict["diff_summary"].is_string());
        assert!(conflict["instructions"].is_string());

        let tracked = std::fs::read_dir(state_dir.join("tracked"))
            .expect("tracked state should exist")
            .count();
        let indexes = std::fs::read_dir(state_dir.join("by-path"))
            .expect("path index should exist")
            .count();
        assert_eq!(tracked, 2, "sidecar and baseline should remain");
        assert_eq!(indexes, 1, "one path index should remain");
        stop_protocol(client, server_task).await;
        fixture.finish();
    }
}
