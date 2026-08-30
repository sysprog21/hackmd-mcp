//! Tools that move a note between `HackMD` and a local Markdown file.

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use super::HackmdServer;
use crate::{
    reply,
    sync::{
        check::CheckNoteSyncInput,
        pull::PullNoteInput,
        push::PushNoteInput,
        snapshot::SaveRemoteSnapshotInput,
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
        description = "Pull one HackMD note's exact Markdown body to an absolute local path and atomically record a private sync baseline. Existing files require overwrite_local: true. For @owner/slug references, refresh=true bypasses the 60-second list cache.",
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
        description = "Push a tracked local Markdown file with safe baseline comparison by default. strategy: overwrite requires confirm: true and replaces unversioned remote content. For @owner/slug references, refresh=true bypasses the 60-second list cache.",
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
            Ok(Ok(output)) => reply::structured("Evaluated tracked HackMD note push", &output),
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
            Ok(output) => reply::structured("Checked tracked HackMD note sync state", &output),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_save_remote_snapshot",
        description = "Atomically save the tracked note's current remote body as sibling *.remote.md without changing the working Markdown file. Existing snapshots require explicit overwrite.",
        annotations(
            title = "Save HackMD Remote Snapshot",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn save_remote_snapshot(
        &self,
        Parameters(input): Parameters<SaveRemoteSnapshotInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::sync::snapshot::save_remote_snapshot(&self.client, &self.files, input).await {
            Ok(output) => reply::structured("Saved HackMD remote snapshot", &output),
            Err(error) => reply::error(error.to_string()),
        }
    }
}
