use rmcp::model::{CallToolResult, ContentBlock, MetaObject};
use serde::Serialize;

/// A successful reply: a one-line summary, the output as JSON text, and the
/// same output as `structuredContent`.
///
/// The JSON text block is what the MCP spec asks of a tool that returns
/// structured content, and it is not redundant in practice: a client that
/// reads only `content` would otherwise see the summary and never the note
/// body, candidates, or diff the call was made for.
///
/// The duplication costs bytes on the local stdio pipe, not model context: a
/// client hands its model one form or the other. Dropping either would break
/// the clients that read only that one, so both stay.
///
/// `output` is taken by value and dropped once converted, so a note body is
/// not held a third time while the text form is rendered.
pub(crate) fn structured<T: Serialize>(summary: impl Into<String>, output: T) -> CallToolResult {
    let structured = serde_json::to_value(&output).expect("tool output should serialize");
    drop(output);
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(summary),
        ContentBlock::text(structured.to_string()),
    ]);
    result.structured_content = Some(structured);
    result
}

/// The reply for a `note_ref` that named no note, or more than one. It is a
/// successful result carrying candidates, not an error, so the caller can pick
/// one and retry.
pub(crate) fn unresolved(resolution: &crate::note::reference::NoteResolution) -> CallToolResult {
    #[derive(Serialize)]
    struct Unresolved<'a> {
        resolution: &'a crate::note::reference::NoteResolution,
    }
    structured(
        "The note reference did not resolve uniquely",
        &Unresolved { resolution },
    )
}

/// A tool's reply from its result: `summary` names a success, and an error
/// keeps its kind.
pub(crate) fn respond<T: Serialize, E: ToolError>(
    result: Result<T, E>,
    summary: impl FnOnce(&T) -> String,
) -> CallToolResult {
    match result {
        Ok(output) => structured(summary(&output), output),
        Err(error) => self::error(&error),
    }
}

/// `respond`, for a tool whose `note_ref` may not name exactly one note.
pub(crate) fn respond_resolved<T: Serialize, E: ToolError>(
    result: Result<Result<T, crate::note::reference::NoteResolution>, E>,
    summary: impl FnOnce(&T) -> String,
) -> CallToolResult {
    match result {
        Ok(Err(resolution)) => unresolved(&resolution),
        Ok(Ok(output)) => structured(summary(&output), output),
        Err(error) => self::error(&error),
    }
}

/// The name a unit enum variant has on the wire, for summaries that echo a
/// status field without keeping a second copy of its spelling.
pub(crate) fn wire_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::new(),
    }
}

/// A stable class for a failed call, reported as `_meta.error_kind` beside
/// the message so an agent can decide what to do next without parsing text.
/// The names are a contract: add to them, never rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorKind {
    /// No token, or `HackMD` rejected it: fix the configuration and restart.
    Auth,
    /// The token lacks permission for this note, folder, or team.
    Forbidden,
    /// The note, folder, or team does not exist for this account.
    NotFound,
    /// The change conflicts with the current state: `HackMD` refused it, such
    /// as a permalink in use, or the body changed since the caller read it.
    /// Re-read before trying again.
    Conflict,
    /// Out of quota; wait for the reset before retrying.
    RateLimited,
    /// A connection failure or timeout; retrying may succeed.
    Network,
    /// `HackMD` failed or answered with something unusable; retry later.
    Upstream,
    /// `HackMD` rejected the request for another reason.
    Api,
    /// The write was sent but could not be confirmed.
    Readback,
    /// The write landed but a follow-up step failed: do not repeat the write.
    PartialWrite,
    /// A body, file, or response is over a size limit.
    TooLarge,
    /// Retrying with the confirmation flag named in the message would proceed.
    ConfirmationRequired,
    /// The local file has edits a pull would discard.
    UnpushedChanges,
    /// The input is malformed or unsupported; change it before retrying.
    InvalidInput,
    /// A patch did not apply to the current note body; re-read and regenerate
    /// it.
    PatchRejected,
    /// A local path is outside what this server may touch.
    LocalAccess,
    /// A local file or the sync store could not be read or written.
    LocalIo,
    /// The local file has no sync record; pull it first.
    NotTracked,
    /// The sync record is damaged or inconsistent; re-pull or untrack.
    SyncState,
    /// The server itself is misconfigured.
    Internal,
}

/// An error a tool can return: its message, and its class.
pub(crate) trait ToolError: std::fmt::Display {
    fn kind(&self) -> ErrorKind;
}

pub(crate) fn error(error: &impl ToolError) -> CallToolResult {
    let mut result = CallToolResult::error(vec![ContentBlock::text(error.to_string())]);
    let kind = serde_json::to_value(error.kind()).expect("error kind serializes");
    result
        .meta
        .get_or_insert_with(MetaObject::default)
        .0
        .insert("error_kind".to_owned(), kind);
    result
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{error, structured};

    #[test]
    fn success_has_summary_json_text_and_structured_json() {
        let output = json!({"id": "note-id", "changed": false});
        let result = structured("Note is unchanged", &output);

        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(output.clone()));
        let text = |index: usize| {
            result.content[index]
                .as_text()
                .expect("success content should be text")
                .text
                .clone()
        };
        assert_eq!(text(0), "Note is unchanged");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text(1)).expect("second block is JSON"),
            output
        );
    }

    #[test]
    fn error_is_a_caller_visible_tool_error_with_its_kind() {
        let result = error(&crate::sync::state::StateError::NotTracked);

        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, None);
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("error content should be text")
                .text,
            "local Markdown file is not tracked; pull it before sync operations"
        );
        assert_eq!(
            result.meta.expect("errors carry _meta").0.get("error_kind"),
            Some(&json!("not_tracked"))
        );
    }
}
