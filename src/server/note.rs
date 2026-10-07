//! Tools that act on notes: the note lists, one note and its trash, and
//! images attached to it.

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use super::HackmdServer;
use crate::{
    note::{
        crud::{CreateNoteInput, DeleteNoteInput, UpdateNoteInput, UpdateNoteOutput},
        get::{GetNoteInput, GetNoteOutput},
        image::UploadNoteImageInput,
        list::ListNotesInput,
    },
    reply,
};

#[tool_router(router = note_router, vis = "pub(crate)")]
impl HackmdServer {
    #[tool(
        name = "hackmd_list_notes",
        description = "List HackMD notes with metadata filtering, sorting, and pagination. source picks the list: workspace (default; a personal or team workspace's notes), history (the account's view history, in HackMD's order), trash (trashed personal notes), or tracked (local files synced by hackmd_pull_note, read without a request; team_path narrows it to one team). This never searches note bodies. refresh=true bypasses the 60-second workspace cache.",
        annotations(
            title = "List HackMD Notes",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn list_notes(
        &self,
        Parameters(input): Parameters<ListNotesInput>,
    ) -> rmcp::model::CallToolResult {
        reply::respond(
            crate::note::list::list_notes(&self.client, &self.files, input).await,
            |output| {
                let meta = output.meta();
                format!(
                    "Found {} matching HackMD note(s); returned {}",
                    meta.total, meta.count
                )
            },
        )
    }

    #[tool(
        name = "hackmd_get_note",
        description = "Get one HackMD note with full content, normalized metadata, folder_ids, the exact patch_path a hackmd_update_note patch needs, and body_hash for its expected_hash. Given local_path instead of note_ref, report that tracked file's sync state without writing: in_sync, local_changed, remote_changed, or conflict, with SHA-256 hashes. For @owner/slug references, refresh=true bypasses the 60-second caches.",
        annotations(
            title = "Get HackMD Note",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn get_note(
        &self,
        Parameters(input): Parameters<GetNoteInput>,
    ) -> rmcp::model::CallToolResult {
        reply::respond_resolved(
            crate::note::get::get_note(&self.client, &self.files, input).await,
            |output| match output {
                GetNoteOutput::Note(note) => format!("Fetched HackMD note {}", note.id),
                GetNoteOutput::Sync(sync) => format!(
                    "HackMD note {} sync state: {}",
                    sync.note_id,
                    reply::wire_name(&sync.status)
                ),
            },
        )
    }

    #[tool(
        name = "hackmd_create_note",
        description = "Create a HackMD note in a personal or team workspace and return its metadata and patch_path (not the body). Folder placement is read back after POST; a compatibility PATCH runs only if the API dropped parentFolderId, resending the body so it is kept. Title precedence: a YAML title: in content wins, then a leading H1, then title. Pass read_permission whenever access matters. Sent tags, description, permalink, and permissions the created note does not show are listed in unconfirmed_fields (no other field is checked); fix them with hackmd_update_note, never by creating the note again.",
        annotations(
            title = "Create HackMD Note",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn create_note(
        &self,
        Parameters(input): Parameters<CreateNoteInput>,
    ) -> rmcp::model::CallToolResult {
        reply::respond(
            crate::note::crud::create_note(&self.client, input).await,
            |output| format!("Created HackMD note {}", output.note.id),
        )
    }

    #[tool(
        name = "hackmd_update_note",
        description = "Edit a HackMD note's body with a patch (the default and safe way), or update its metadata, or replace the whole body with content. The destructive hint is for content; a patch is context-checked and changes only the body lines its hunks name. It applies only when every hunk's context matches the current body exactly once, then the write is confirmed by reading it back. Pass expected_hash (body_hash from hackmd_get_note) with any update to refuse the write if the body changed since you read it. A metadata-only update reads the current body and sends it back with the change: an edit landing between that read and the write is reverted, and expected_hash catches any made before it. Title precedence: a YAML title: in the body wins, then a leading H1, then title, so on a note with either a title change has no effect. Format:\n*** Begin Patch\n*** Update File: <patch_path from hackmd_get_note>\n@@ optional anchor line\n context line\n-removed line\n+added line\n*** End Patch\nText after @@ is an anchor that must equal exactly one line, ignoring leading and trailing whitespace; the hunk then applies after it, and an addition-only hunk is inserted directly below it. A line range such as @@ -3,4 +3,5 @@ is not an anchor. A hunk closed by *** End of File must match the end of the body, and an addition-only one appends there. A patch goes in a call of its own. content overwrites the complete body, and nothing this server offers can undo it. For @owner/slug references, refresh=true bypasses the 60-second caches.",
        annotations(
            title = "Update HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn update_note(
        &self,
        Parameters(input): Parameters<UpdateNoteInput>,
    ) -> rmcp::model::CallToolResult {
        reply::respond_resolved(
            crate::note::crud::update_note(&self.client, input).await,
            |output| match output {
                UpdateNoteOutput::Updated { note, .. } => {
                    format!("Updated HackMD note {}", note.id)
                }
                UpdateNoteOutput::Patched(edit) if edit.changed => {
                    format!("Edited HackMD note {}", edit.note_id)
                }
                UpdateNoteOutput::Patched(edit) => {
                    format!("HackMD note {} is unchanged", edit.note_id)
                }
            },
        )
    }

    #[tool(
        name = "hackmd_delete_note",
        description = "Delete a HackMD note, or with restore: true bring a personal note back from trash (note_ref is then the internal ID from hackmd_list_notes with source: trash). Only personal notes can be restored through the API. For @owner/slug references, refresh=true bypasses the 60-second caches.",
        annotations(
            title = "Delete or Restore HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn delete_note(
        &self,
        Parameters(input): Parameters<DeleteNoteInput>,
    ) -> rmcp::model::CallToolResult {
        reply::respond_resolved(
            crate::note::crud::delete_note(&self.client, input).await,
            |output| {
                let verb = if output.restored {
                    "Restored"
                } else {
                    "Deleted"
                };
                format!("{verb} HackMD note {}", output.note_id)
            },
        )
    }

    #[tool(
        name = "hackmd_upload_note_image",
        description = "Upload an image to a HackMD note (personal or team) and return its HackMD CDN link. Give image_path for a local file, or image_url to re-host an image from a public https URL (such as an imgur link in a note being migrated) without saving it locally first. publicly_readable is what one signed-out HEAD of the link showed right after the upload: true when an image or a redirect to presigned storage came back, false when it was refused, null when the check could not run or proved nothing. A false usually means the note is not guest-readable, and signed-out readers will not see the image until it is. A local image must lie under HACKMD_MCP_WORKSPACE_ROOT; with no root configured, local uploads are refused. image_url is fetched only over https on the default port from a host whose every address is public, following at most 5 redirects. Either way the bytes must be a PNG, JPEG, GIF, or WebP image. Files above 5 MiB require confirmation; files above 10 MiB are refused. For @owner/slug references, refresh=true bypasses the 60-second caches.",
        annotations(
            title = "Upload HackMD Note Image",
            read_only_hint = false,
            // Publishing a file at a public link cannot be taken back.
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn upload_note_image(
        &self,
        Parameters(input): Parameters<UploadNoteImageInput>,
    ) -> rmcp::model::CallToolResult {
        reply::respond_resolved(
            crate::note::image::upload_note_image(&self.client, &self.files, input).await,
            |output| {
                let readers = match output.publicly_readable {
                    Some(true) => "a signed-out request for the link was served",
                    Some(false) => {
                        "a signed-out request for the link was refused, usually because the note is not guest-readable"
                    }
                    None => "whether signed-out readers can open the link is unknown",
                };
                format!("Uploaded HackMD note image; {readers}")
            },
        )
    }
}
