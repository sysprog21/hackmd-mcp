use thiserror::Error;

use crate::models::Workspace;

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
    #[error("patch operation is not supported by hackmd_update_note: {0}")]
    UnsupportedOperation(String),
    #[error("patch must contain at least one @@ hunk after the Update File header")]
    MissingHunk,
    #[error("patch hunk is malformed: {0}")]
    MalformedHunk(&'static str),
    #[error("patch hunk context was not found")]
    ContextNotFound,
    #[error("patch hunk context matched multiple locations")]
    AmbiguousContext,
    #[error("patch hunk anchor @@ {0} matches no line")]
    AnchorNotFound(String),
    #[error("patch hunk anchor @@ {0} matches more than one line")]
    AmbiguousAnchor(String),
}

impl crate::reply::ToolError for PatchError {
    fn kind(&self) -> crate::reply::ErrorKind {
        crate::reply::ErrorKind::PatchRejected
    }
}

#[derive(Debug)]
struct FilePatch {
    target: String,
    hunks: Vec<Hunk>,
}

/// One `@@` section. Text after `@@` is an anchor: a line the note must
/// contain exactly once, compared with surrounding whitespace ignored, and
/// after which the hunk applies. It is how a caller
/// disambiguates context that also appears earlier in the note.
#[derive(Debug)]
struct Hunk {
    anchor: Option<String>,
    lines: Vec<HunkLine>,
    /// Closed by `*** End of File`: the hunk's old lines must be the last
    /// lines of the body, and an addition-only hunk appends to it.
    at_end: bool,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

/// The identifier a patch must name in its `*** Update File:` header.
///
/// Deliberately not URL-encoded: this is a patch target, not a route. The
/// caller only has to reproduce the exact string `hackmd_get_note` handed it,
/// and the header is compared verbatim, so a patch prepared for one note can
/// never be applied to another.
pub(crate) fn patch_path(workspace: &Workspace, note_id: &str) -> String {
    match workspace {
        Workspace::Personal => format!("notes/{note_id}.md"),
        Workspace::Team { team_path } => format!("teams/{team_path}/notes/{note_id}.md"),
    }
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

    let (mut lines, endings) = split_lines(content);
    for hunk in file_patch.hunks {
        apply_hunk(&mut lines, hunk)?;
    }
    Ok(join_lines(&lines, endings))
}

/// Line endings of the body being patched. A body that uses one style keeps it.
/// A body that mixes both is rejoined with whichever most of its lines use
/// (LF on a tie), because rejoining has to pick one: a single pasted CRLF line
/// must not rewrite every ending of an LF note.
#[derive(Clone, Copy)]
struct LineEndings {
    crlf: bool,
    trailing_newline: bool,
}

/// Splits a body into newline-free lines, which is what hunk lines are: they
/// reach us through `str::lines`, with any `\r` already stripped. Splitting the
/// body the same way is what lets CRLF context match at all.
fn split_lines(content: &str) -> (Vec<String>, LineEndings) {
    let endings = LineEndings {
        crlf: {
            let crlf = content.matches("\r\n").count();
            crlf > content.matches('\n').count() - crlf
        },
        trailing_newline: content.ends_with('\n'),
    };
    (content.lines().map(ToOwned::to_owned).collect(), endings)
}

fn join_lines(lines: &[String], endings: LineEndings) -> String {
    let separator = if endings.crlf { "\r\n" } else { "\n" };
    let mut output = lines.join(separator);
    // A body whose every line was removed is empty, not a lone newline.
    if endings.trailing_newline && !lines.is_empty() {
        output.push_str(separator);
    }
    output
}

/// Checks the envelope and the single `*** Update File:` header, returning
/// the target and the hunk lines after it.
fn update_section<'a>(lines: &'a [&'a str]) -> Result<(&'a str, &'a [&'a str]), PatchError> {
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
    Ok((target, &body[1..]))
}

fn parse_patch(patch: &str) -> Result<FilePatch, PatchError> {
    let lines = patch.lines().collect::<Vec<_>>();
    let (target, hunk_lines) = update_section(&lines)?;

    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;

    // A bare empty line is an empty context line whose leading space was
    // trimmed, which editors and models both do routinely. It only counts
    // between two hunk lines: blank padding before a hunk's first line or after
    // its last cannot demand blank lines the note does not have, nor turn an
    // addition-only hunk into one that needs context.
    let mut pending_blank_lines = 0;
    for line in hunk_lines {
        if let Some(anchor) = line
            .strip_prefix("@@")
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            if let Some(hunk) = current.take() {
                push_hunk(&mut hunks, hunk)?;
            }
            let anchor = anchor.trim();
            current = Some(Hunk {
                anchor: (!anchor.is_empty() && !is_line_range(anchor)).then(|| anchor.to_owned()),
                lines: Vec::new(),
                at_end: false,
            });
            pending_blank_lines = 0;
            continue;
        }
        if line.is_empty() {
            pending_blank_lines += 1;
            continue;
        }

        // Codex-style patches close a hunk that reaches the end of the body. It
        // must follow a hunk's lines, and ties that hunk to the end.
        if *line == "*** End of File" {
            // It ends the hunk it closes, so a line after it has no hunk to
            // join and is refused as such.
            match current.take() {
                Some(mut hunk) if !hunk.lines.is_empty() => {
                    hunk.at_end = true;
                    push_hunk(&mut hunks, hunk)?;
                }
                _ => {
                    return Err(PatchError::MalformedHunk(
                        "*** End of File must close a hunk that has lines",
                    ));
                }
            }
            pending_blank_lines = 0;
            continue;
        }
        let Some(Hunk { lines: hunk, .. }) = current.as_mut() else {
            return Err(PatchError::MissingHunk);
        };
        let blank_lines = std::mem::take(&mut pending_blank_lines);
        if !hunk.is_empty() {
            hunk.extend((0..blank_lines).map(|_| HunkLine::Context(String::new())));
        }
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
    if let Some(hunk) = current {
        push_hunk(&mut hunks, hunk)?;
    }
    if hunks.is_empty() {
        return Err(PatchError::MissingHunk);
    }
    Ok(FilePatch {
        target: target.to_owned(),
        hunks,
    })
}

/// Whether the text after `@@` is a unified-diff line range such as
/// `-3,4 +3,5 @@`. Models write these out of habit; they locate nothing here,
/// so such a hunk is simply unanchored.
fn is_line_range(text: &str) -> bool {
    let range = |part: &str, sign: char| {
        part.strip_prefix(sign).is_some_and(|numbers| {
            !numbers.is_empty()
                && numbers.split(',').all(|number| {
                    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
    };
    let mut parts = text.split_whitespace();
    matches!(
        (parts.next(), parts.next()),
        (Some(old), Some(new)) if range(old, '-') && range(new, '+')
    ) && parts.next().is_none_or(|rest| rest.starts_with("@@"))
}

fn push_hunk(hunks: &mut Vec<Hunk>, hunk: Hunk) -> Result<(), PatchError> {
    if hunk.lines.is_empty() {
        return Err(PatchError::MalformedHunk("hunk cannot be empty"));
    }
    hunks.push(hunk);
    Ok(())
}

/// Whether a run of body lines is exactly the hunk's old side.
fn window_eq(window: &[String], old: &[&str]) -> bool {
    window.iter().map(String::as_str).eq(old.iter().copied())
}

fn apply_hunk(lines: &mut Vec<String>, hunk: Hunk) -> Result<(), PatchError> {
    // Search only from the anchor line on, so context that also appears before
    // the anchor no longer makes the hunk ambiguous.
    let start = match hunk.anchor.as_deref() {
        Some(anchor) => {
            unique_index(lines.iter().map(|line| line.trim() == anchor)).map_err(|found| {
                match found {
                    Found::None => PatchError::AnchorNotFound(anchor.to_owned()),
                    Found::Many => PatchError::AmbiguousAnchor(anchor.to_owned()),
                }
            })?
        }
        None => 0,
    };

    // Located before the replacement is built, so the old side is borrowed
    // straight from the hunk rather than copied.
    let old = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(value) | HunkLine::Remove(value) => Some(value.as_str()),
            HunkLine::Add(_) => None,
        })
        .collect::<Vec<_>>();
    let replaced = if old.is_empty() {
        None
    } else if hunk.at_end {
        // Only the window that ends the body can match.
        let position = lines
            .len()
            .checked_sub(old.len())
            .filter(|&position| position >= start && window_eq(&lines[position..], &old))
            .ok_or(PatchError::ContextNotFound)?;
        Some((position, old.len()))
    } else {
        let offset = unique_index(
            lines[start..]
                .windows(old.len())
                .map(|window| window_eq(window, &old)),
        )
        .map_err(|found| match found {
            Found::None => PatchError::ContextNotFound,
            Found::Many => PatchError::AmbiguousContext,
        })?;
        Some((start + offset, old.len()))
    };
    let new = hunk
        .lines
        .into_iter()
        .filter_map(|line| match line {
            HunkLine::Context(value) | HunkLine::Add(value) => Some(value),
            HunkLine::Remove(_) => None,
        })
        .collect::<Vec<_>>();

    match replaced {
        Some((position, len)) => {
            lines.splice(position..position + len, new);
        }
        // An addition closed by `*** End of File` appends to the body.
        None if hunk.at_end => lines.extend(new),
        // An anchored addition goes directly below its anchor line.
        None if hunk.anchor.is_some() => {
            let below = start + 1;
            lines.splice(below..below, new);
        }

        // Nothing to place it by: an unanchored addition-only hunk is
        // unambiguous only when it fills an empty note, which is the one body
        // no context can describe.
        None if lines.is_empty() => *lines = new,
        None => {
            return Err(PatchError::MalformedHunk(
                "an addition-only hunk applies only to an empty note; add an @@ anchor, context, or removed lines",
            ));
        }
    }
    Ok(())
}

enum Found {
    None,
    Many,
}

/// The index of the one `true` in `matches`: a patch never guesses between
/// candidates, for anchors and context alike.
fn unique_index(matches: impl Iterator<Item = bool>) -> Result<usize, Found> {
    let mut hits = matches
        .enumerate()
        .filter_map(|(index, hit)| hit.then_some(index));
    let first = hits.next().ok_or(Found::None)?;
    if hits.next().is_some() {
        return Err(Found::Many);
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
    fn a_bare_empty_line_inside_a_hunk_is_empty_context() {
        let patch = envelope("@@\n one\n\n-three\n+3\n");
        assert_eq!(
            apply_note_patch("one\n\nthree\n", &patch, TARGET),
            Ok("one\n\n3\n".to_owned())
        );
    }

    #[test]
    fn an_anchor_disambiguates_context_repeated_above_it() {
        let body = "## A\n- item\n## B\n- item\n";
        assert_eq!(
            apply_note_patch(body, &envelope("@@\n-- item\n+- changed"), TARGET),
            Err(PatchError::AmbiguousContext)
        );
        assert_eq!(
            apply_note_patch(body, &envelope("@@ ## B\n-- item\n+- changed"), TARGET),
            Ok("## A\n- item\n## B\n- changed\n".to_owned())
        );
    }

    #[test]
    fn a_unified_diff_line_range_is_not_an_anchor() {
        for header in [
            "@@ -1,2 +1,2 @@",
            "@@ -1 +1 @@",
            "@@ -1,2 +1,2 @@ ## Heading",
        ] {
            assert_eq!(
                apply_note_patch("a\nb\n", &envelope(&format!("{header}\n-a\n+x")), TARGET),
                Ok("x\nb\n".to_owned()),
                "{header}"
            );
        }
        assert!(!super::is_line_range("-1,2 +1,2 extra"));
        assert!(!super::is_line_range("## Heading"));
    }

    #[test]
    fn blank_padding_around_hunks_is_not_context() {
        // Before the first @@, and between an anchor and an addition.
        let patch =
            "*** Begin Patch\n*** Update File: notes/a.md\n\n@@ ## B\n\n+new\n*** End Patch";
        assert_eq!(
            apply_note_patch("## B\nold\n", patch, TARGET),
            Ok("## B\nnew\nold\n".to_owned())
        );
    }

    #[test]
    fn an_anchored_addition_goes_directly_below_the_anchor() {
        assert_eq!(
            apply_note_patch("# T\n## B\nold\n", &envelope("@@ ## B\n+new"), TARGET),
            Ok("# T\n## B\nnew\nold\n".to_owned())
        );
    }

    #[test]
    fn a_missing_or_repeated_anchor_is_an_error() {
        assert_eq!(
            apply_note_patch("a\nb\n", &envelope("@@ c\n-a\n+x"), TARGET),
            Err(PatchError::AnchorNotFound("c".to_owned()))
        );
        assert_eq!(
            apply_note_patch("a\na\nb\n", &envelope("@@ a\n-b\n+x"), TARGET),
            Err(PatchError::AmbiguousAnchor("a".to_owned()))
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
    fn empty_note_accepts_an_addition_only_hunk() {
        let patch = envelope("@@\n+first\n+second");
        assert_eq!(
            apply_note_patch("", &patch, TARGET),
            Ok("first\nsecond".to_owned())
        );
    }

    #[test]
    fn crlf_body_matches_context_and_keeps_its_line_endings() {
        let patch = envelope("@@\n-old\n+new");
        assert_eq!(
            apply_note_patch("intro\r\nold\r\n", &patch, TARGET),
            Ok("intro\r\nnew\r\n".to_owned())
        );
    }

    #[test]
    fn a_mixed_ending_body_takes_the_ending_most_lines_use() {
        let patch = envelope("@@\n-old\n+new");
        assert_eq!(
            apply_note_patch("a\nb\r\nold\n", &patch, TARGET),
            Ok("a\nb\nnew\n".to_owned())
        );
        assert_eq!(
            apply_note_patch("a\r\nb\nold\r\n", &patch, TARGET),
            Ok("a\r\nb\r\nnew\r\n".to_owned())
        );
        // A tie keeps LF.
        assert_eq!(
            apply_note_patch("intro\r\nold\n", &patch, TARGET),
            Ok("intro\nnew\n".to_owned())
        );
    }

    #[test]
    fn an_end_of_file_hunk_matches_only_the_end_of_the_body() {
        let patch = envelope("@@\n-old\n+new\n*** End of File");
        assert_eq!(
            apply_note_patch("intro\nold\n", &patch, TARGET),
            Ok("intro\nnew\n".to_owned())
        );
        // The same line elsewhere is not the end.
        assert_eq!(
            apply_note_patch("old\nlast\n", &patch, TARGET),
            Err(PatchError::ContextNotFound)
        );
        // Two copies are not ambiguous when only one ends the body.
        assert_eq!(
            apply_note_patch("old\nold\n", &patch, TARGET),
            Ok("old\nnew\n".to_owned())
        );
    }

    #[test]
    fn an_end_of_file_addition_appends() {
        let patch = envelope("@@\n+tail\n*** End of File");
        assert_eq!(
            apply_note_patch("body\n", &patch, TARGET),
            Ok("body\ntail\n".to_owned())
        );
    }

    #[test]
    fn an_end_of_file_marker_out_of_place_is_refused() {
        for body in [
            "*** End of File\n@@\n+x",
            "@@\n*** End of File",
            "@@\n+x\n*** End of File\n+y",
        ] {
            assert!(
                matches!(
                    apply_note_patch("body\n", &envelope(body), TARGET),
                    Err(PatchError::MalformedHunk(_) | PatchError::MissingHunk)
                ),
                "{body:?}"
            );
        }
    }

    #[test]
    fn removing_every_line_leaves_an_empty_body() {
        let patch = envelope("@@\n-only");
        assert_eq!(
            apply_note_patch("only\n", &patch, TARGET),
            Ok(String::new())
        );
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
                PatchError::MalformedHunk(
                    "an addition-only hunk applies only to an empty note; add an @@ anchor, context, or removed lines",
                ),
            ),
        ];
        for (patch, expected) in cases {
            assert_eq!(apply_note_patch("old", patch, TARGET), Err(expected));
        }
    }
}
