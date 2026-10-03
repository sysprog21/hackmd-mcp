//! Account and workspace discovery: the profile `hackmd_get_me` returns.

use rmcp::schemars;
use serde::Deserialize;

use crate::{
    client::{HackmdClient, HackmdError},
    dto::ProfileResponse,
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyInput {}

/// The profile with its teams filled in. `/me` may carry them already; when
/// it does not, one `/teams` request supplies them. Both are fetched fresh:
/// this is the call an agent makes to see the account as it is now.
pub(crate) async fn get_me(client: &HackmdClient) -> Result<ProfileResponse, HackmdError> {
    let mut profile = client.get_me().await?;
    if profile.teams.is_empty() {
        profile.teams = client.list_teams(true).await?.to_vec();
    }
    Ok(profile)
}

pub(crate) fn profile_summary(profile: &ProfileResponse) -> String {
    format!(
        "Authenticated as {} (user_path: {}); {} team(s)",
        profile.name,
        profile.user_path,
        profile.teams.len()
    )
}

#[cfg(test)]
mod tests {
    use crate::fixture::{Scenario, SequenceServer};

    #[tokio::test]
    async fn teams_missing_from_the_profile_come_from_one_teams_request() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new(
                "GET",
                "/v1/me",
                200,
                r#"{"id":"u","name":"User","userPath":"alice"}"#,
            ),
            Scenario::new(
                "GET",
                "/v1/teams",
                200,
                r#"[{"id":"t","name":"Core","path":"core"}]"#,
            ),
        ]);
        let profile = super::get_me(&fixture.client())
            .await
            .expect("profile should load");
        assert_eq!(profile.teams.len(), 1);
        assert_eq!(
            super::profile_summary(&profile),
            "Authenticated as User (user_path: alice); 1 team(s)"
        );
        fixture.finish();
    }

    #[tokio::test]
    async fn teams_already_in_the_profile_cost_no_second_request() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/me",
            200,
            r#"{"id":"u","name":"User","userPath":"alice","teams":[{"id":"t","name":"Core","path":"core"}]}"#,
        )]);
        let profile = super::get_me(&fixture.client())
            .await
            .expect("profile should load");
        assert_eq!(profile.teams.len(), 1);
        assert_eq!(fixture.finish().len(), 1);
    }
}
