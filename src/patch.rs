use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum PatchError {
    #[error("patch must start with *** Begin Patch")]
    MissingBegin,
    #[error("patch must end with *** End Patch")]
    MissingEnd,
    #[error("patch must contain exactly one non-empty *** Update File section before its hunks")]
    InvalidUpdateSection,
    #[error("patch targets {actual}, expected {expected}")]
    WrongTarget { actual: String, expected: String },
    #[error("patch operation is not supported by hackmd_edit_note: {0}")]
    UnsupportedOperation(String),
    #[error("patch must contain at least one @@ hunk after the Update File header")]
    MissingHunk,
    #[error("patch hunk is malformed: {0}")]
    MalformedHunk(&'static str),
    #[error("patch hunk context was not found")]
    ContextNotFound,
    #[error("patch hunk context matched multiple locations")]
    AmbiguousContext,
}

#[derive(Debug)]
struct FilePatch {
    target: String,
    hunks: Vec<Hunk>,
}

#[derive(Debug)]
struct Hunk {
    lines: Vec<HunkLine>,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

pub(crate) fn apply_note_patch(
    content: &str,
    patch: &str,
    expected_target: &str,
) -> Result<String, PatchError> {
    let file_patch = parse_patch(patch)?;
    if file_patch.target != expected_target {
        return Err(PatchError::WrongTarget {
            actual: file_patch.target,
            expected: expected_target.to_owned(),
        });
    }

    let trailing_newline = content.ends_with('\n');
    let mut lines = content
        .strip_suffix('\n')
        .unwrap_or(content)
        .split('\n')
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if content.is_empty() {
        lines.clear();
    }
    for hunk in file_patch.hunks {
        apply_hunk(&mut lines, hunk)?;
    }

    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    Ok(output)
}

fn parse_patch(patch: &str) -> Result<FilePatch, PatchError> {
    let lines = patch.lines().collect::<Vec<_>>();
    if lines.first() != Some(&"*** Begin Patch") {
        return Err(PatchError::MissingBegin);
    }
    if lines.last() != Some(&"*** End Patch") {
        return Err(PatchError::MissingEnd);
    }

    let body = &lines[1..lines.len() - 1];
    for line in body {
        if line.starts_with("*** Add File:")
            || line.starts_with("*** Delete File:")
            || line.starts_with("*** Move to:")
        {
            return Err(PatchError::UnsupportedOperation((*line).to_owned()));
        }
    }
    let Some(first) = body.first() else {
        return Err(PatchError::InvalidUpdateSection);
    };
    let Some(target) = first.strip_prefix("*** Update File: ") else {
        return Err(PatchError::InvalidUpdateSection);
    };
    if target.is_empty()
        || body
            .iter()
            .skip(1)
            .any(|line| line.starts_with("*** Update File:"))
    {
        return Err(PatchError::InvalidUpdateSection);
    }

    let mut hunks = Vec::new();
    let mut current: Option<Vec<HunkLine>> = None;
    for line in body.iter().skip(1) {
        if *line == "@@" || line.starts_with("@@ ") {
            if let Some(lines) = current.take() {
                validate_hunk(&lines)?;
                hunks.push(Hunk { lines });
            }
            current = Some(Vec::new());
            continue;
        }
        let Some(hunk) = current.as_mut() else {
            return Err(PatchError::MissingHunk);
        };
        if let Some(value) = line.strip_prefix(' ') {
            hunk.push(HunkLine::Context(value.to_owned()));
        } else if let Some(value) = line.strip_prefix('+') {
            hunk.push(HunkLine::Add(value.to_owned()));
        } else if let Some(value) = line.strip_prefix('-') {
            hunk.push(HunkLine::Remove(value.to_owned()));
        } else {
            return Err(PatchError::MalformedHunk(
                "each line must start with space, +, or -",
            ));
        }
    }
    let Some(lines) = current else {
        return Err(PatchError::MissingHunk);
    };
    validate_hunk(&lines)?;
    hunks.push(Hunk { lines });
    Ok(FilePatch {
        target: target.to_owned(),
        hunks,
    })
}

fn validate_hunk(lines: &[HunkLine]) -> Result<(), PatchError> {
    if lines.is_empty() {
        return Err(PatchError::MalformedHunk("hunk cannot be empty"));
    }
    if !lines
        .iter()
        .any(|line| matches!(line, HunkLine::Context(_) | HunkLine::Remove(_)))
    {
        return Err(PatchError::MalformedHunk(
            "hunk must include context or removed lines",
        ));
    }
    Ok(())
}

fn apply_hunk(lines: &mut Vec<String>, hunk: Hunk) -> Result<(), PatchError> {
    let old = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(value) | HunkLine::Remove(value) => Some(value.clone()),
            HunkLine::Add(_) => None,
        })
        .collect::<Vec<_>>();
    let new = hunk
        .lines
        .into_iter()
        .filter_map(|line| match line {
            HunkLine::Context(value) | HunkLine::Add(value) => Some(value),
            HunkLine::Remove(_) => None,
        })
        .collect::<Vec<_>>();
    let position = find_unique_match(lines, &old)?;
    lines.splice(position..position + old.len(), new);
    Ok(())
}

fn find_unique_match(lines: &[String], needle: &[String]) -> Result<usize, PatchError> {
    let mut matches = lines
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| (window == needle).then_some(index));
    let Some(first) = matches.next() else {
        return Err(PatchError::ContextNotFound);
    };
    if matches.next().is_some() {
        return Err(PatchError::AmbiguousContext);
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::{PatchError, apply_note_patch};

    const TARGET: &str = "notes/a.md";

    fn envelope(body: &str) -> String {
        format!("*** Begin Patch\n*** Update File: {TARGET}\n{body}\n*** End Patch")
    }

    #[test]
    fn applies_multiple_hunks_and_preserves_trailing_newline_state() {
        let patch = envelope("@@\n-one\n+1\n@@\n-three\n+3");
        assert_eq!(
            apply_note_patch("one\ntwo\nthree\n", &patch, TARGET),
            Ok("1\ntwo\n3\n".to_owned())
        );
        assert_eq!(
            apply_note_patch("one\ntwo\nthree", &patch, TARGET),
            Ok("1\ntwo\n3".to_owned())
        );
    }

    #[test]
    fn context_only_hunk_is_a_valid_no_op() {
        let patch = envelope("@@\n only line");
        assert_eq!(
            apply_note_patch("only line", &patch, TARGET),
            Ok("only line".to_owned())
        );
    }

    #[test]
    fn rejects_wrong_missing_and_ambiguous_context_distinctly() {
        let wrong = envelope("@@\n-old\n+new").replace(TARGET, "notes/b.md");
        assert!(matches!(
            apply_note_patch("old", &wrong, TARGET),
            Err(PatchError::WrongTarget { .. })
        ));
        let patch = envelope("@@\n-old\n+new");
        assert_eq!(
            apply_note_patch("other", &patch, TARGET),
            Err(PatchError::ContextNotFound)
        );
        assert_eq!(
            apply_note_patch("old\nold", &patch, TARGET),
            Err(PatchError::AmbiguousContext)
        );
    }

    #[test]
    fn rejects_unsupported_file_operations() {
        for operation in ["Add File", "Delete File", "Move to"] {
            let patch = format!(
                "*** Begin Patch\n*** {operation}: {TARGET}\n@@\n-old\n+new\n*** End Patch"
            );
            assert!(matches!(
                apply_note_patch("old", &patch, TARGET),
                Err(PatchError::UnsupportedOperation(_))
            ));
        }
    }

    #[test]
    fn rejects_malformed_envelopes_and_hunks_distinctly() {
        let cases = [
            (
                "*** Update File: notes/a.md\n@@\n-old\n+new\n*** End Patch",
                PatchError::MissingBegin,
            ),
            (
                "*** Begin Patch\n*** Update File: notes/a.md\n@@\n-old\n+new",
                PatchError::MissingEnd,
            ),
            (
                "*** Begin Patch\n*** Update File: notes/a.md\n*** Update File: notes/a.md\n@@\n-old\n+new\n*** End Patch",
                PatchError::InvalidUpdateSection,
            ),
            (
                "*** Begin Patch\n*** Update File: notes/a.md\n-old\n+new\n*** End Patch",
                PatchError::MissingHunk,
            ),
            (
                "*** Begin Patch\n*** Update File: notes/a.md\n@@\ninvalid\n*** End Patch",
                PatchError::MalformedHunk("each line must start with space, +, or -"),
            ),
            (
                "*** Begin Patch\n*** Update File: notes/a.md\n@@\n+only-add\n*** End Patch",
                PatchError::MalformedHunk("hunk must include context or removed lines"),
            ),
        ];
        for (patch, expected) in cases {
            assert_eq!(apply_note_patch("old", patch, TARGET), Err(expected));
        }
    }
}
