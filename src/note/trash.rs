use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    client::{HackmdClient, HackmdError},
    models::Workspace,
    note::list::{ListNotesOutput, note_summary, page_summaries},
    paging::{InvalidLimit, default_limit, validate_limit},
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListTrashInput {
    /// Maximum trashed notes returned (default 20, maximum 100).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub(crate) limit: usize,
    /// Number of trashed notes to skip in API order.
    #[serde(default)]
    pub(crate) offset: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestoreNoteInput {
    /// Internal ID of a personal note currently in trash.
    pub(crate) note_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct RestoreNoteOutput {
    pub(crate) note_id: String,
    pub(crate) restored: bool,
    pub(crate) response: Option<Value>,
}

#[derive(Debug, Error)]
pub(crate) enum TrashError {
    #[error(transparent)]
    Limit(#[from] InvalidLimit),
    #[error("note_id must not be empty")]
    EmptyNoteId,
    #[error(transparent)]
    Api(#[from] HackmdError),
}

pub(crate) async fn list_trash(
    client: &HackmdClient,
    input: ListTrashInput,
) -> Result<ListNotesOutput, TrashError> {
    validate_limit(input.limit)?;
    let notes = client
        .list_trash()
        .await?
        .into_iter()
        .map(|note| note_summary(note, Workspace::Personal))
        .collect();
    Ok(page_summaries(notes, input.offset, input.limit))
}

pub(crate) async fn restore_note(
    client: &HackmdClient,
    input: RestoreNoteInput,
) -> Result<RestoreNoteOutput, TrashError> {
    if input.note_id.trim().is_empty() {
        return Err(TrashError::EmptyNoteId);
    }
    let response = client.restore_note(&input.note_id).await?;
    Ok(RestoreNoteOutput {
        note_id: input.note_id,
        restored: true,
        response,
    })
}

#[cfg(test)]
mod tests {
    use super::{ListTrashInput, RestoreNoteInput, TrashError, list_trash, restore_note};
    use crate::{client::HackmdClient, config::Config, fixture::SequenceServer};

    #[tokio::test]
    async fn list_trash_is_slim_paginated_and_accepts_empty_lists() {
        let fixture = SequenceServer::spawn([
            (
                200,
                r#"[{"id":"a","title":"A","content":"secret","lastChangedAt":2},{"id":"b","title":"B","lastChangedAt":1}]"#,
            ),
            (200, "[]"),
        ]);
        let client = fixture.client();
        let page = list_trash(
            &client,
            ListTrashInput {
                limit: 1,
                offset: 1,
            },
        )
        .await
        .expect("trash page should succeed");
        assert_eq!(page.meta.total, 2);
        assert_eq!(page.meta.count, 1);
        assert_eq!(page.notes[0].id, "b");
        assert_eq!(page.notes[0].workspace, crate::models::Workspace::Personal);
        let serialized = serde_json::to_value(&page).expect("page should serialize");
        assert!(serialized["notes"][0].get("content").is_none());

        let empty = list_trash(
            &client,
            ListTrashInput {
                limit: 20,
                offset: 0,
            },
        )
        .await
        .expect("empty trash should succeed");
        assert_eq!(empty.meta.total, 0);
        assert_eq!(empty.meta.next_offset, None);
        assert!(
            fixture
                .finish()
                .iter()
                .all(|request| request.starts_with("GET /v1/trash HTTP/1.1\r\n"))
        );
    }

    #[tokio::test]
    async fn restore_encodes_id_and_accepts_empty_or_json_responses() {
        let fixture = SequenceServer::spawn([(202, ""), (200, r#"{"restored":true}"#)]);
        let client = fixture.client();
        let accepted = restore_note(
            &client,
            RestoreNoteInput {
                note_id: "folder/id".to_owned(),
            },
        )
        .await
        .expect("empty accepted restore should succeed");
        assert!(accepted.restored);
        assert!(accepted.response.is_none());
        let json = restore_note(
            &client,
            RestoreNoteInput {
                note_id: "other".to_owned(),
            },
        )
        .await
        .expect("JSON restore should succeed");
        assert_eq!(json.response.expect("response")["restored"], true);
        let requests = fixture.finish();
        assert!(requests[0].starts_with("PUT /v1/trash/folder%2Fid/restore HTTP/1.1\r\n"));
        assert!(requests[1].starts_with("PUT /v1/trash/other/restore HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn invalid_inputs_are_rejected_before_network() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
        assert!(matches!(
            list_trash(
                &client,
                ListTrashInput {
                    limit: 0,
                    offset: 0
                }
            )
            .await,
            Err(TrashError::Limit(_))
        ));
        assert!(matches!(
            restore_note(
                &client,
                RestoreNoteInput {
                    note_id: " ".to_owned()
                }
            )
            .await,
            Err(TrashError::EmptyNoteId)
        ));
    }
}
