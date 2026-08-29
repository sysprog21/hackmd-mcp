//! Tools that act on notes: the list, one note, its history, its trash, and
//! images attached to it.

use rmcp::{handler::server::wrapper::Parameters, tool, tool_router};

use super::HackmdServer;
use crate::{
    note::{
        crud::{CreateNoteInput, DeleteNoteInput, UpdateNoteInput},
        edit::EditNoteInput,
        get::GetNoteInput,
        history::HistoryInput,
        image::UploadNoteImageInput,
        list::ListNotesInput,
        trash::{ListTrashInput, RestoreNoteInput},
    },
    reply,
};

#[tool_router(router = note_router, vis = "pub(crate)")]
impl HackmdServer {
    #[tool(
        name = "hackmd_list_notes",
        description = "List personal or team HackMD notes with local metadata filtering, deterministic sorting, and pagination. This does not search note content.",
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
        match crate::note::list::list_notes(&self.client, input).await {
            Ok(output) => reply::success(
                format!(
                    "Found {} matching HackMD note(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                serde_json::to_value(output).expect("list-notes output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_get_note",
        description = "Get one HackMD note with full content, normalized metadata, folder_ids, and the exact patch_path for safe edits.",
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
        match crate::note::get::get_note(&self.client, input).await {
            Ok(Ok(note)) => reply::success(
                format!("Fetched HackMD note {}", note.id),
                serde_json::json!({"note": note}),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_create_note",
        description = "Create a HackMD note in a personal or team workspace. Folder placement is read back after POST; a compatibility PATCH runs only if the API dropped parentFolderId.",
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
        match crate::note::crud::create_note(&self.client, input).await {
            Ok(output) => reply::success(
                format!("Created HackMD note {}", output.note.id),
                serde_json::json!({"result": output}),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_update_note",
        description = "Fallback note update for metadata or an explicit full content replacement. Prefer hackmd_edit_note for normal body edits because content here overwrites the complete unversioned body.",
        annotations(
            title = "Update HackMD Note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn update_note(
        &self,
        Parameters(input): Parameters<UpdateNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::crud::update_note(&self.client, input).await {
            Ok(Ok(output)) => reply::success(
                format!("HackMD accepted the update for note {}", output.note_id),
                serde_json::json!({"result": output}),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_delete_note",
        description = "Delete a HackMD note. Personal deletion moves it to recoverable trash; team restore is not exposed. This remains destructive.",
        annotations(
            title = "Delete HackMD Note",
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
        match crate::note::crud::delete_note(&self.client, input).await {
            Ok(Ok(output)) => reply::success(
                format!("Deleted HackMD note {}", output.note_id),
                serde_json::json!({"result": output}),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_list_trash",
        description = "List trashed personal HackMD notes with slim metadata and client-side pagination.",
        annotations(
            title = "List Trashed HackMD Notes",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn list_trash(
        &self,
        Parameters(input): Parameters<ListTrashInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::trash::list_trash(&self.client, input).await {
            Ok(output) => reply::success(
                format!(
                    "Found {} trashed HackMD note(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                serde_json::to_value(output).expect("trash-list output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_restore_note",
        description = "Restore a personal HackMD note from trash by internal note ID.",
        annotations(
            title = "Restore Trashed HackMD Note",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn restore_note(
        &self,
        Parameters(input): Parameters<RestoreNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::trash::restore_note(&self.client, input).await {
            Ok(output) => reply::success(
                format!("Restored HackMD note {}", output.note_id),
                serde_json::json!({"result": output}),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_edit_note",
        description = "Default tool for normal HackMD body edits. Applies one strict Codex patch to the current content only when every hunk context is unique; prefer this over hackmd_update_note for body changes.",
        annotations(
            title = "Edit HackMD Note Safely",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn edit_note(
        &self,
        Parameters(input): Parameters<EditNoteInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::edit::edit_note(&self.client, input).await {
            Ok(Ok(output)) => {
                let summary = if output.changed {
                    format!("Edited HackMD note {}", output.note_id)
                } else {
                    format!("HackMD note {} is unchanged", output.note_id)
                };
                reply::success(summary, serde_json::json!({"result": output}))
            }
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_get_history",
        description = "Get recently viewed HackMD notes in API history order with slim metadata and client-side pagination.",
        annotations(
            title = "Get HackMD Browse History",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn get_history(
        &self,
        Parameters(input): Parameters<HistoryInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::history::get_history(&self.client, input).await {
            Ok(output) => reply::success(
                format!(
                    "Found {} HackMD history item(s); returned {}",
                    output.meta.total, output.meta.count
                ),
                serde_json::to_value(output).expect("history output should serialize"),
            ),
            Err(error) => reply::error(error.to_string()),
        }
    }

    #[tool(
        name = "hackmd_upload_note_image",
        description = "Upload a local image to a personal-workspace note and return only its HackMD CDN link. Files above 5 MiB require confirmation; files above 10 MiB are refused.",
        annotations(
            title = "Upload HackMD Note Image",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn upload_note_image(
        &self,
        Parameters(input): Parameters<UploadNoteImageInput>,
    ) -> rmcp::model::CallToolResult {
        match crate::note::image::upload_note_image(&self.client, &self.files, input).await {
            Ok(Ok(output)) => reply::success(
                "Uploaded HackMD note image",
                serde_json::to_value(output).expect("image-upload output should serialize"),
            ),
            Ok(Err(resolution)) => reply::unresolved(&resolution),
            Err(error) => reply::error(error.to_string()),
        }
    }
}
