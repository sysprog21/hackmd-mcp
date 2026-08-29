//! Account and workspace discovery tools.

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use super::{EmptyInput, HackmdServer, profile_result, teams_result};

#[tool_router(router = account_router, vis = "pub(crate)")]
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
    pub(crate) async fn get_me(
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
    pub(crate) async fn list_teams(
        &self,
        Parameters(EmptyInput {}): Parameters<EmptyInput>,
    ) -> rmcp::model::CallToolResult {
        match self.client.list_teams().await {
            Ok(teams) => teams_result(&teams),
            Err(error) => error.into(),
        }
    }
}
