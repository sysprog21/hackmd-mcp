use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::client::{HackmdClient, HackmdError};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RestoreNoteInput {
    /// Internal ID of a personal note in trash, as listed by
    /// `hackmd_list_notes` with source `trash`.
    pub(crate) note_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct RestoreNoteOutput {
    pub(crate) note_id: String,
}

#[derive(Debug, Error)]
pub(crate) enum TrashError {
    #[error("note_id must not be empty")]
    EmptyNoteId,
    #[error(transparent)]
    Api(#[from] HackmdError),
}

impl crate::reply::ToolError for TrashError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::EmptyNoteId => ErrorKind::InvalidInput,
            Self::Api(error) => error.kind(),
        }
    }
}

pub(crate) async fn restore_note(
    client: &HackmdClient,
    input: RestoreNoteInput,
) -> Result<RestoreNoteOutput, TrashError> {
    if input.note_id.trim().is_empty() {
        return Err(TrashError::EmptyNoteId);
    }
    client.restore_note(&input.note_id).await?;
    Ok(RestoreNoteOutput {
        note_id: input.note_id,
    })
}

#[cfg(test)]
mod tests {
    use super::{RestoreNoteInput, TrashError, restore_note};
    use crate::{
        client::HackmdClient,
        config::Config,
        fixture::{Scenario, SequenceServer},
    };

    #[tokio::test]
    async fn restore_encodes_id_and_accepts_empty_or_json_responses() {
        let fixture = SequenceServer::spawn_scenarios([
            Scenario::new("PUT", "/v1/trash/folder%2Fid/restore", 202, ""),
            Scenario::new(
                "PUT",
                "/v1/trash/other/restore",
                200,
                r#"{"restored":true}"#,
            ),
        ]);
        let client = fixture.client();
        for note_id in ["folder/id", "other"] {
            let restored = restore_note(
                &client,
                RestoreNoteInput {
                    note_id: note_id.to_owned(),
                },
            )
            .await
            .expect("restore should succeed");
            assert_eq!(restored.note_id, note_id);
        }
        fixture.finish();
    }

    #[tokio::test]
    async fn empty_note_id_is_rejected_before_network() {
        let client = HackmdClient::new(Config::for_tests()).expect("client should build");
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
