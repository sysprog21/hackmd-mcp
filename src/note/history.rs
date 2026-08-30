use crate::{
    client::HackmdClient,
    models::Workspace,
    note::list::{ListNotesError, ListNotesOutput, note_summary, page_summaries},
    paging::{default_limit, validate_limit},
};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryInput {
    /// Maximum history entries returned (default 20, maximum 100).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub(crate) limit: usize,
    /// Number of history entries to skip.
    #[serde(default)]
    pub(crate) offset: usize,
}

pub(crate) async fn get_history(
    client: &HackmdClient,
    input: HistoryInput,
) -> Result<ListNotesOutput, ListNotesError> {
    validate_limit(input.limit)?;
    let notes = client
        .get_history()
        .await?
        .into_iter()
        .map(|note| {
            let workspace = note
                .team_path
                .as_ref()
                .map_or(Workspace::Personal, |team_path| Workspace::Team {
                    team_path: team_path.clone(),
                });
            note_summary(note, workspace)
        })
        .collect();
    Ok(page_summaries(notes, input.offset, input.limit))
}

#[cfg(test)]
mod tests {
    use super::{HistoryInput, get_history};
    use crate::note::list::ListNotesError;
    use crate::{
        client::HackmdClient,
        config::Config,
        fixture::{Scenario, SequenceServer},
    };

    #[tokio::test]
    async fn accepts_bare_history_and_preserves_api_order_and_last_visit() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/history",
            200,
            r#"[
                {"id":"newest","title":"Newest","lastVisit":30},
                {"id":"older","title":"Older","lastVisit":20,"teamPath":"core"}
            ]"#,
        )]);
        let client = fixture.client();
        let output = get_history(
            &client,
            HistoryInput {
                limit: 1,
                offset: 1,
            },
        )
        .await
        .expect("history should succeed");
        assert_eq!(output.meta.total, 2);
        assert_eq!(output.notes[0].id, "older");
        assert_eq!(output.notes[0].last_visit, Some(20));
        assert!(matches!(
            output.notes[0].workspace,
            crate::models::Workspace::Team { ref team_path } if team_path == "core"
        ));
        fixture.finish();
    }

    #[tokio::test]
    async fn accepts_wrapped_history_and_validates_limit_before_network() {
        let fixture = SequenceServer::spawn_scenarios([Scenario::new(
            "GET",
            "/v1/history",
            200,
            r#"{"history":[{"id":"one","title":"One","lastVisit":1}]}"#,
        )]);
        let client = fixture.client();
        let output = get_history(
            &client,
            HistoryInput {
                limit: 20,
                offset: 0,
            },
        )
        .await
        .expect("wrapped history should succeed");
        assert_eq!(output.meta.count, 1);
        fixture.finish();

        let no_token = HackmdClient::new(Config::for_tests()).expect("client should build");
        assert!(matches!(
            get_history(
                &no_token,
                HistoryInput {
                    limit: 0,
                    offset: 0
                }
            )
            .await,
            Err(ListNotesError::Limit(_))
        ));
    }
}
