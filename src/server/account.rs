//! Account and workspace discovery.

use rmcp::{handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use super::HackmdServer;
use crate::{client::HackmdError, dto::ProfileResponse, reply};

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
        match self.account().await {
            Ok(profile) => profile_result(&profile),
            Err(error) => reply::error(&error),
        }
    }
}

impl HackmdServer {
    /// The profile with its teams filled in. `/me` may carry them already; when
    /// it does not, one `/teams` request supplies them. Both are fetched fresh:
    /// this is the call an agent makes to see the account as it is now.
    async fn account(&self) -> Result<ProfileResponse, HackmdError> {
        let mut profile = self.client.get_me().await?;
        if profile.teams.is_empty() {
            profile.teams = self.client.list_teams(true).await?.to_vec();
        }
        Ok(profile)
    }
}

pub(crate) fn profile_result(profile: &ProfileResponse) -> rmcp::model::CallToolResult {
    let summary = format!(
        "Authenticated as {} (user_path: {}); {} team(s)",
        profile.name,
        profile.user_path,
        profile.teams.len()
    );
    reply::structured(summary, profile)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyInput {}
