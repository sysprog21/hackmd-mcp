//! Account and workspace discovery.

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use super::HackmdServer;
use crate::{
    account::{EmptyInput, get_me, profile_summary},
    reply,
};

#[tool_router(router = account_router, vis = "pub(crate)")]
impl HackmdServer {
    #[tool(
        name = "hackmd_get_me",
        description = "Get the authenticated HackMD profile and its teams. Use user_path to recognize personal @owner/slug URLs, and pass a team's path as team_path to work in that team.",
        annotations(
            title = "Get HackMD Profile and Teams",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn get_me(
        &self,
        Parameters(EmptyInput {}): Parameters<EmptyInput>,
    ) -> rmcp::model::CallToolResult {
        reply::respond(get_me(&self.client).await, profile_summary)
    }
}
