use std::path::PathBuf;

use rmcp::schemars;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    local::LocalFiles,
    models::Workspace,
    paging::{InvalidLimit, PageMeta, default_limit, paginate, validate_limit},
    sync::state::StateError,
};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListTrackedNotesInput {
    /// Maximum tracked notes returned (default 20, maximum 100).
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 100))]
    pub(crate) limit: usize,
    /// Number of tracked notes to skip.
    #[serde(default)]
    pub(crate) offset: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct TrackedNoteSummary {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    pub(crate) local_path: PathBuf,
    pub(crate) baseline_hash: String,
    pub(crate) last_observed_remote_timestamp: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ListTrackedNotesOutput {
    #[serde(flatten)]
    pub(crate) meta: PageMeta,
    pub(crate) notes: Vec<TrackedNoteSummary>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UntrackNoteInput {
    /// Team path (from `hackmd_get_me`) of a team workspace; omit for the
    /// personal workspace.
    #[serde(default, rename = "team_path", alias = "workspace")]
    pub(crate) workspace: Workspace,
    /// Internal `HackMD` note ID shown by `hackmd_list_tracked_notes`.
    pub(crate) note_id: String,
    /// Required because private baseline and sidecar files will be deleted.
    #[serde(default)]
    pub(crate) confirm: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct UntrackNoteOutput {
    #[serde(rename = "team_path")]
    pub(crate) workspace: Workspace,
    pub(crate) note_id: String,
    /// The Markdown file the record pointed at, left exactly as it was.
    pub(crate) local_path: PathBuf,
}

#[derive(Debug, Error)]
pub(crate) enum TrackingError {
    #[error(transparent)]
    Limit(#[from] InvalidLimit),
    #[error("untracking requires confirm: true; the Markdown file is preserved")]
    ConfirmationRequired,
    #[error("note_id must be non-empty")]
    EmptyNoteId,
    #[error(transparent)]
    State(#[from] StateError),
}

impl crate::reply::ToolError for TrackingError {
    fn kind(&self) -> crate::reply::ErrorKind {
        use crate::reply::ErrorKind;

        match self {
            Self::Limit(..) | Self::EmptyNoteId => ErrorKind::InvalidInput,
            Self::ConfirmationRequired => ErrorKind::ConfirmationRequired,
            Self::State(error) => error.kind(),
        }
    }
}

pub(crate) fn list_tracked_notes(
    files: &LocalFiles,
    input: &ListTrackedNotesInput,
) -> Result<ListTrackedNotesOutput, TrackingError> {
    validate_limit(input.limit)?;
    let mut notes = files
        .state()
        .list_tracked()?
        .into_iter()
        .map(|state| TrackedNoteSummary {
            workspace: state.workspace,
            note_id: state.internal_id,
            local_path: state.local_path,
            baseline_hash: state.baseline_body_hash,
            last_observed_remote_timestamp: state.last_observed_remote_timestamp,
        })
        .collect::<Vec<_>>();
    notes.sort_by(|left, right| {
        (&left.workspace, &left.note_id).cmp(&(&right.workspace, &right.note_id))
    });
    let (notes, meta) = paginate(notes, input.offset, input.limit);
    Ok(ListTrackedNotesOutput { meta, notes })
}

pub(crate) fn untrack_note(
    files: &LocalFiles,
    input: &UntrackNoteInput,
) -> Result<UntrackNoteOutput, TrackingError> {
    if input.note_id.trim().is_empty() {
        return Err(TrackingError::EmptyNoteId);
    }
    if !input.confirm {
        return Err(TrackingError::ConfirmationRequired);
    }
    let state = files.state().untrack(&input.workspace, &input.note_id)?;
    Ok(UntrackNoteOutput {
        workspace: state.workspace,
        note_id: state.internal_id,
        local_path: state.local_path,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        ListTrackedNotesInput, TrackingError, UntrackNoteInput, list_tracked_notes, untrack_note,
    };
    use crate::{
        local::LocalFiles,
        models::Workspace,
        sync::state::{StateError, TrackedNoteState},
    };

    fn tracked_files() -> (tempfile::TempDir, LocalFiles, std::path::PathBuf) {
        let directory = tempfile::tempdir().expect("temporary directory should create");
        let local_path = directory.path().join("note.md");
        fs::write(&local_path, "baseline").expect("working Markdown should write");
        let files = LocalFiles::new(directory.path().join("state"), None);
        let state = TrackedNoteState::capture(
            "note-id".to_owned(),
            Workspace::Personal,
            local_path.clone(),
            "baseline",
            Some(1234),
        )
        .expect("tracked state should capture");
        files
            .state()
            .persist_from_sync(&state, "baseline")
            .expect("tracked state should persist");
        (directory, files, local_path)
    }

    #[test]
    fn list_is_slim_sorted_and_paginated_without_reading_working_files() {
        let (_directory, files, local_path) = tracked_files();
        fs::remove_file(&local_path).expect("working file should be removable");

        let output = list_tracked_notes(
            &files,
            &ListTrackedNotesInput {
                limit: 20,
                offset: 0,
            },
        )
        .expect("tracked state should list without the working file");

        assert_eq!(output.meta.total, 1);
        assert_eq!(output.notes[0].note_id, "note-id");
        assert_eq!(output.notes[0].local_path, local_path);
        assert_eq!(output.notes[0].baseline_hash.len(), 71);
        assert_eq!(output.notes[0].last_observed_remote_timestamp, "1234");
    }

    #[test]
    fn untrack_requires_confirmation_and_preserves_the_markdown_file() {
        let (directory, files, local_path) = tracked_files();
        let refused = untrack_note(
            &files,
            &UntrackNoteInput {
                workspace: Workspace::Personal,
                note_id: "note-id".to_owned(),
                confirm: false,
            },
        );
        assert!(matches!(refused, Err(TrackingError::ConfirmationRequired)));

        let output = untrack_note(
            &files,
            &UntrackNoteInput {
                workspace: Workspace::Personal,
                note_id: "note-id".to_owned(),
                confirm: true,
            },
        )
        .expect("confirmed untrack should succeed");
        assert_eq!(output.local_path, local_path);
        assert_eq!(
            fs::read_to_string(&local_path).expect("Markdown should remain"),
            "baseline"
        );
        assert!(matches!(
            files.state().load_for_local_path(&local_path),
            Err(StateError::NotTracked)
        ));
        assert_eq!(
            fs::read_dir(directory.path().join("state/tracked"))
                .expect("tracked directory should remain")
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(directory.path().join("state/by-path"))
                .expect("index directory should remain")
                .count(),
            0
        );
    }

    #[test]
    fn stale_state_can_be_untracked_after_the_markdown_was_deleted() {
        let (_directory, files, local_path) = tracked_files();
        fs::remove_file(local_path).expect("working Markdown should be removable");

        let output = untrack_note(
            &files,
            &UntrackNoteInput {
                workspace: Workspace::Personal,
                note_id: "note-id".to_owned(),
                confirm: true,
            },
        )
        .expect("stale state should remain removable");
        assert_eq!(output.note_id, "note-id");
    }

    #[test]
    fn invalid_inputs_do_not_change_state() {
        let (_directory, files, _local_path) = tracked_files();
        assert!(matches!(
            list_tracked_notes(
                &files,
                &ListTrackedNotesInput {
                    limit: 101,
                    offset: 0
                }
            ),
            Err(TrackingError::Limit(_))
        ));
        assert!(matches!(
            untrack_note(
                &files,
                &UntrackNoteInput {
                    workspace: Workspace::Personal,
                    note_id: " ".to_owned(),
                    confirm: true,
                }
            ),
            Err(TrackingError::EmptyNoteId)
        ));
        assert_eq!(
            list_tracked_notes(
                &files,
                &ListTrackedNotesInput {
                    limit: 20,
                    offset: 0
                }
            )
            .expect("state should remain")
            .meta
            .total,
            1
        );
    }
}
