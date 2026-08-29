use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

pub(crate) fn success(summary: impl Into<String>, structured: Value) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(summary)]);
    result.structured_content = Some(structured);
    result
}

/// A successful reply whose structured half is the tool's own output type.
pub(crate) fn structured<T: serde::Serialize>(
    summary: impl Into<String>,
    output: &T,
) -> CallToolResult {
    success(
        summary,
        serde_json::to_value(output).expect("tool output should serialize"),
    )
}

/// The reply for a `note_ref` that named no note, or more than one. It is a
/// successful result carrying candidates, not an error, so the caller can pick
/// one and retry.
pub(crate) fn unresolved(resolution: &crate::note::reference::NoteResolution) -> CallToolResult {
    success(
        "The note reference did not resolve uniquely",
        serde_json::json!({"resolution": resolution}),
    )
}

pub(crate) fn error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{error, success};

    #[test]
    fn success_has_concise_text_and_structured_json() {
        let structured = json!({"id": "note-id", "changed": false});
        let result = success("Note is unchanged", structured.clone());

        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(structured));
        assert_eq!(
            result.content[0]
                .as_text()
                .expect("success content should be text")
                .text,
            "Note is unchanged"
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
