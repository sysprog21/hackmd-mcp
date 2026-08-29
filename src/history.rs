use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    list_notes::{DEFAULT_LIMIT, ListNotesOutput, MAX_LIMIT, note_summary, page_summaries},
    models::Workspace,
};

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

const fn default_limit() -> usize {
    DEFAULT_LIMIT
}

#[derive(Debug, Error)]
pub(crate) enum HistoryError {
    #[error("limit must be between 1 and {MAX_LIMIT}")]
    InvalidLimit,
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn get_history(
    client: &HackmdClient,
    input: HistoryInput,
) -> Result<ListNotesOutput, HistoryError> {
    if !(1..=MAX_LIMIT).contains(&input.limit) {
        return Err(HistoryError::InvalidLimit);
    }
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
    use super::{HistoryError, HistoryInput, get_history};
    use crate::{client::HackmdClient, config::Config, test_support::SequenceServer};

    #[tokio::test]
    async fn accepts_bare_history_and_preserves_api_order_and_last_visit() {
        let fixture = SequenceServer::spawn([(
            200,
            r#"[
                {"id":"newest","title":"Newest","lastVisit":30},
                {"id":"older","title":"Older","lastVisit":20,"teamPath":"core"}
            ]"#,
        )]);
        let client = HackmdClient::new(Config::for_loopback_test(
            &fixture.api_url,
            Some("fixture-token"),
        ))
        .expect("fixture client should build");
        let output = get_history(
            &client,
            HistoryInput {
                limit: 1,
                offset: 1,
            },
        )
        .await
        .expect("history should succeed");
        assert_eq!(output.total, 2);
        assert_eq!(output.notes[0].id, "older");
        assert_eq!(output.notes[0].last_visit, Some(20));
        assert!(matches!(
            output.notes[0].workspace,
            crate::models::Workspace::Team { ref team_path } if team_path == "core"
        ));
        assert!(fixture.finish()[0].starts_with("GET /v1/history HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn accepts_wrapped_history_and_validates_limit_before_network() {
        let fixture = SequenceServer::spawn([(
            200,
            r#"{"history":[{"id":"one","title":"One","lastVisit":1}]}"#,
        )]);
        let client = HackmdClient::new(Config::for_loopback_test(
            &fixture.api_url,
            Some("fixture-token"),
        ))
        .expect("fixture client should build");
        let output = get_history(
            &client,
            HistoryInput {
                limit: 20,
                offset: 0,
            },
        )
        .await
        .expect("wrapped history should succeed");
        assert_eq!(output.count, 1);
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
            Err(HistoryError::InvalidLimit)
        ));
    }
}
