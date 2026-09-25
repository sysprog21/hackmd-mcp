use rmcp::model::{CallToolResult, ContentBlock};
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
pub(crate) fn structured<T: Serialize>(summary: impl Into<String>, output: &T) -> CallToolResult {
    let structured = serde_json::to_value(output).expect("tool output should serialize");
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

/// The name a unit enum variant has on the wire, for summaries that echo a
/// status field without keeping a second copy of its spelling.
pub(crate) fn wire_name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::new(),
    }
}

pub(crate) fn error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
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
    fn error_is_a_caller_visible_tool_error() {
        let result = error("note_ref did not resolve");

        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, None);
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("error content should be text")
                .text,
            "note_ref did not resolve"
        );
    }
}
