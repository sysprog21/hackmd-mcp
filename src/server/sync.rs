//! Tools that move a note between `HackMD` and a local Markdown file.

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use super::HackmdServer;
use crate::{
    reply,
    sync::{
        check::CheckNoteSyncInput,
        pull::PullNoteInput,
        push::PushNoteInput,
        tracking::{ListTrackedNotesInput, UntrackNoteInput},
    },
};

#[tool_router(router = sync_router, vis = "pub(crate)")]
impl HackmdServer {
    #[tool(
        name = "hackmd_list_tracked_notes",
        description = "List private local HackMD sync records with workspace, note ID, Markdown path, baseline hash, and last observed remote timestamp. This reads no note bodies and makes no remote request.",
        annotations(
            title = "List Tracked HackMD Notes",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn list_tracked_notes(
        &self,
        Parameters(input): Parameters<ListTrackedNotesInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::tracking::list_tracked_notes(&self.files, &input) {
            Ok(output) => reply::structured(
                format!(
                    "Found {} tracked HackMD note(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                &output,
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_untrack_note",
        description = "Delete one private sync sidecar, exact baseline, and path-index hint. Requires confirm=true and never deletes or changes the local Markdown file or remote HackMD note.",
        annotations(
            title = "Untrack HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn untrack_note(
        &self,
        Parameters(input): Parameters<UntrackNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::tracking::untrack_note(&self.files, &input) {
            Ok(output) => reply::structured(
                format!("Stopped tracking HackMD note {}", output.note_id),
                &output,
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_pull_note",
        description = "Pull one HackMD note's exact Markdown body to an absolute local .md path and record a private sync baseline, which starts tracking the file. An existing file needs overwrite_local: true, and a tracked file with unpushed edits also needs discard_local_changes: true. For @owner/slug references, refresh=true bypasses the 60-second caches.",
        annotations(
            title = "Pull HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn pull_note(
        &self,
        Parameters(input): Parameters<PullNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::pull::pull_note(&self.client, &self.files, input).await {
            Ok(Ok(output)) => {
                reply::structured(format!("Pulled HackMD note {}", output.note_id), &output)
            }
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_push_note",
        description = "Push a tracked local Markdown file to the note it was pulled from. The default safe strategy writes only if the remote still matches the last-synced baseline; otherwise it returns remote_changed, or conflict with a bounded diff, the current remote body saved as a sibling *.remote.md, and remote_body_hash. After merging, push again with expected_remote_hash to write only if the remote has not moved. strategy: overwrite requires confirm: true and replaces unversioned remote content.",
        annotations(
            title = "Push HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn push_note(
        &self,
        Parameters(input): Parameters<PushNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::push::push_note(&self.client, &self.files, input).await {
            Ok(Ok(output)) => reply::structured(
                format!(
                    "Push of HackMD note {}: {}",
                    output.note_id,
                    reply::wire_name(&output.status)
                ),
                &output,
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_check_note_sync",
        description = "Read local, private baseline, and remote note state without writing, returning in_sync, remote_changed, local_changed, or conflict plus SHA-256 hashes.",
        annotations(
            title = "Check HackMD Note Sync",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn check_note_sync(
        &self,
        Parameters(input): Parameters<CheckNoteSyncInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::check::check_note_sync(&self.client, &self.files, input).await {
            Ok(output) => reply::structured(
                format!(
                    "HackMD note {} sync state: {}",
                    output.note_id,
                    reply::wire_name(&output.status)
                ),
                &output,
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }
}
