//! A note body in the form `HackMD` stores it, and the title it implies.

use std::ops::ControlFlow;

/// A body in the form `HackMD` stores it. Measured 2026-10-08: `\r\n` and a
/// lone `\r` read back as `\n`, while a BOM, trailing whitespace, and trailing
/// newlines are kept byte for byte. Hashing or comparing any other form
/// against a read-back could never match.
pub(crate) fn into_stored(body: String) -> String {
    if !body.contains('\r') {
        return body;
    }
    let mut normalized = String::with_capacity(body.len());
    let mut parts = body.split('\r');
    normalized.push_str(parts.next().unwrap_or_default());
    for part in parts {
        normalized.push('\n');
        normalized.push_str(part.strip_prefix('\n').unwrap_or(part));
    }
    normalized
}

/// The title `body` implies when it differs from the note's listed `title`.
/// `HackMD` derives a title from the body only when a note is created without
/// one (front-matter `title:`, then the first H1, then "Untitled"); later body
/// edits never change it, and an explicit `title` always wins (measured
/// 2026-10-08). So an edited H1 leaves the listing showing the old title.
pub(crate) fn title_drift(title: &str, body: &str) -> Option<String> {
    // Whether `HackMD` collapses runs of spaces or tabs is unmeasured, so a
    // difference in those alone is never reported.
    body_title(body).filter(|implied| !implied.split_whitespace().eq(title.split_whitespace()))
}

/// A longer title is not compared at all, which also bounds the work a
/// hostile heading can cause.
const TITLE_MAX_BYTES: usize = 1024;

/// Front matter is a few lines; one this long is not looked through for its
/// end, so a body that opens with `---` costs a bounded scan.
const FRONT_MATTER_MAX_LINES: usize = 256;

/// A front-matter `title:`, else an H1 that is the body's first line of
/// text, with `[text](url)` reduced to its text. Only that shape is read:
/// anything before the H1 (a paragraph, fence, list, quote, HTML, a reference
/// definition, a setext heading) may hold or hide the title in ways this does
/// not parse, so it gives none, as does any YAML or inline markup this does
/// not decode. A missed drift costs nothing; a wrong one renames a note.
fn body_title(body: &str) -> Option<String> {
    let body = body.strip_prefix('\u{feff}').unwrap_or(body);
    let mut lines = body.lines().peekable();
    if lines.next_if_eq(&"---").is_some() {
        // Unclosed, the `---` is a thematic break, which is not an H1.
        if !lines
            .clone()
            .take(FRONT_MATTER_MAX_LINES)
            .any(|line| line == "---")
        {
            return None;
        }
        let front = lines.by_ref().take_while(|line| *line != "---");
        if let ControlFlow::Break(title) = front_matter_title(front) {
            return title;
        }
    }
    let first = lines.find(|line| !line.trim().is_empty())?;
    let heading = first.trim_start_matches(' ');
    if first.len() - heading.len() > 3 {
        return None;
    }
    atx_h1(heading).and_then(heading_title)
}

/// The title an H1's text gives, with links reduced to their text. Code
/// spans, escapes, emphasis, strikethrough, marks, inline HTML, and entities
/// are not decoded, so a heading with any of them gives none. Markup is
/// checked after links, so a URL's `_` or `&` is no concern.
fn heading_title(heading: &str) -> Option<String> {
    if heading.len() > TITLE_MAX_BYTES || heading.contains(['`', '\\']) {
        return None;
    }
    let text = link_text(heading)?;
    let markup = text.contains(['*', '_', '~', '<', '&', '[', ']']) || text.contains("==");
    (!text.is_empty() && !markup).then_some(text)
}

/// What front matter says of the title. A title key owns the title whether
/// or not its value can be read for certain, so it breaks with that value
/// (none when unreadable) and the H1 is not asked; without one it continues.
/// Only plain `key: value` lines, blanks, comments, and the indented or `-`
/// lines of a key other than `title` are read: any other line breaks with
/// none, since it may hide a title or make the whole block something else.
fn front_matter_title<'a>(front: impl Iterator<Item = &'a str>) -> ControlFlow<Option<String>> {
    let (mut follows, mut title) = (Follows::Nothing, ControlFlow::Continue(()));
    for line in front {
        let content = line.trim_start();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        // YAML indents with spaces only, so a tab there leaves the block
        // unparsed, title and all.
        if line[..line.len() - content.len()].contains('\t') {
            return ControlFlow::Break(None);
        }
        if line.starts_with([' ', '-']) {
            // A title continued this way is folded, multi-line, or a list;
            // after anything else that cannot take it, or with no key above,
            // the block is YAML that does not parse.
            let fits = match follows {
                Follows::Nested => true,
                Follows::Block => !line.starts_with('-'),
                Follows::Plain => !line.starts_with('-') && plain_text(content).is_some(),
                Follows::Nothing => false,
            };
            if !fits {
                return ControlFlow::Break(None);
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return ControlFlow::Break(None);
        };
        let plain_key = !key.is_empty()
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
        if !plain_key
            || !(value.is_empty() || value.starts_with([' ', '\t']))
            || !ends_on_its_line(value)
        {
            return ControlFlow::Break(None);
        }
        follows = Follows::after(key, value);
        if key == "title" {
            if title.is_break() {
                return ControlFlow::Break(None);
            }
            title = ControlFlow::Break(yaml_scalar(value));
        }
    }
    title
}

/// What may follow a key's line: nested lines under an empty value, a block
/// scalar's indented text, a plain scalar's folded continuation, or nothing
/// (after the title, a closed quote, or a flow collection).
#[derive(Clone, Copy)]
enum Follows {
    Nested,
    Block,
    Plain,
    Nothing,
}

impl Follows {
    fn after(key: &str, value: &str) -> Self {
        let value = value.trim();
        if key == "title" {
            Self::Nothing
        } else if value.is_empty() || value.starts_with('#') {
            Self::Nested
        } else if value.starts_with(['|', '>']) {
            Self::Block
        } else if value.starts_with(['"', '\'', '[', '{']) {
            Self::Nothing
        } else {
            Self::Plain
        }
    }
}

/// Whether a value is one this reads for certain to end on its line: empty,
/// a comment, a block scalar's header, a closed quote, a flow collection
/// closed here without quotes, or a plain scalar that is not itself a
/// mapping, each with at most a comment after it. Anything else (a tag, an
/// anchor, an open quote or bracket) may run over lines that look like keys,
/// or make the whole block YAML that does not parse.
fn ends_on_its_line(value: &str) -> bool {
    let value = value.trim();
    let Some(first) = value.chars().next() else {
        return true;
    };
    let rest = &value[first.len_utf8()..];
    match first {
        '#' => true,
        '|' | '>' => only_comment(
            rest.trim_start_matches(['+', '-', '1', '2', '3', '4', '5', '6', '7', '8', '9']),
        ),
        '"' | '\'' => quote_end(first, rest).is_some_and(only_comment),
        '[' | '{' => closed_flow(value).is_some_and(only_comment),
        '!' | '&' | '*' | '%' | '@' | '`' => false,
        // A block entry or mapping indicator cannot start a plain scalar.
        '-' | '?' | ':' if rest.is_empty() || rest.starts_with([' ', '\t']) => false,
        _ => plain_text(value).is_some(),
    }
}

/// Whether what follows a value on its line is nothing or a comment, whose
/// `#` needs a space or tab before it.
fn only_comment(rest: &str) -> bool {
    let comment = rest.trim_start_matches([' ', '\t']);
    comment.is_empty() || (comment.len() < rest.len() && comment.starts_with('#'))
}

/// What follows the quote closing a scalar opened by `quote`, given `rest`,
/// the line after that opening quote; none when it does not close there and
/// may run over lines that look like keys. A `\` escapes inside `"`, and `''`
/// is a quote inside `'`, so neither closes it.
fn quote_end(quote: char, rest: &str) -> Option<&str> {
    let mut chars = rest.chars();
    while let Some(char) = chars.next() {
        if quote == '"' && char == '\\' {
            chars.next();
        } else if char == quote {
            if quote == '"' || !chars.as_str().starts_with('\'') {
                return Some(chars.as_str());
            }
            chars.next();
        }
    }
    None
}

/// What follows a flow collection closed on this line; none when it is not
/// closed here, its brackets do not match, or it holds quotes, which this does
/// not track.
fn closed_flow(value: &str) -> Option<&str> {
    if value.contains(['"', '\'']) {
        return None;
    }
    let mut open = Vec::new();
    for (index, char) in value.char_indices() {
        let opener = match char {
            '[' | '{' => {
                open.push(char);
                continue;
            }
            ']' => '[',
            '}' => '{',
            _ => continue,
        };
        if open.pop() != Some(opener) {
            return None;
        }
        if open.is_empty() {
            return Some(&value[index + 1..]);
        }
    }
    None
}

/// A plain scalar up to its comment; none when it is a mapping (`a: b`, `a:`)
/// rather than a scalar.
fn plain_text(value: &str) -> Option<&str> {
    let end = value
        .match_indices([' ', '\t'])
        .find(|(index, _)| value[index + 1..].starts_with('#'))
        .map_or(value.len(), |(index, _)| index);
    let text = value[..end].trim_end();
    let mapping = text.contains(": ") || text.contains(":\t") || text.ends_with(':');
    (!mapping).then_some(text)
}

/// A YAML scalar as a title: a quoted value with at most a comment after
/// it, or a plain value up to a comment. Escapes, block scalars,
/// collections, anchors, aliases, tags, and a bare comment give none.
fn yaml_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    let text = match *value.as_bytes().first()? {
        quote @ (b'"' | b'\'') => {
            let inner = &value[1..];
            let rest = quote_end(char::from(quote), inner).filter(|rest| only_comment(rest))?;
            let text = &inner[..inner.len() - rest.len() - 1];
            // A `\` or a doubled `'` is an escape, which is not decoded.
            if text.contains('\\') || text.contains("''") {
                return None;
            }
            text
        }
        b'|' | b'>' | b'[' | b'{' | b'&' | b'*' | b'!' | b'%' | b'@' | b'`' | b'#' | b'~' => {
            return None;
        }
        // A number, date, or the like: YAML may not read it as a string.
        b'0'..=b'9' | b'+' | b'-' | b'.' => return None,
        _ => {
            let text = plain_text(value)?;
            // These words are not strings to YAML.
            let not_string = matches!(
                text.to_ascii_lowercase().as_str(),
                "null" | "true" | "false" | "yes" | "no" | "on" | "off" | "y" | "n"
            );
            if not_string {
                return None;
            }
            text
        }
    };
    (!text.trim().is_empty() && text.len() <= TITLE_MAX_BYTES).then(|| text.to_owned())
}

/// The text of an ATX H1, without a closing run of `#` that follows a space.
fn atx_h1(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('#')?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let rest = rest.trim();
    let open = rest.trim_end_matches('#');
    Some(if open.is_empty() {
        open
    } else if open.ends_with([' ', '\t']) {
        open.trim_end()
    } else {
        rest
    })
}

/// `markdown` with each `[text](url)` reduced to its text; none when a link
/// destination holds whitespace, control characters, or angle brackets, or
/// when bare brackets may be a reference link.
fn link_text(markdown: &str) -> Option<String> {
    let mut text = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(open) = rest.find('[') {
        if rest[..open].ends_with('!') {
            return None;
        }
        text.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        // Brackets nested in the text are markup `heading_title` refuses, so
        // the first `]` is the only close worth trying.
        let link = after
            .find(']')
            .filter(|close| after[close + 1..].starts_with('('))
            .and_then(|close| {
                closing(&after[close + 2..], b'(', b')').map(|end| (close, close + 3 + end))
            });
        if let Some((close, end)) = link {
            // `<...>` destinations follow other rules, and whitespace or a
            // control character means a link title or no link at all.
            let destination = &after[close + 2..end - 1];
            let invalid =
                |c: char| c.is_whitespace() || c.is_ascii_control() || matches!(c, '<' | '>');
            if destination.contains(invalid) {
                return None;
            }
            text.push_str(&after[..close]);
            rest = &after[end..];
        } else {
            // Bare brackets may resolve through a definition elsewhere.
            return None;
        }
    }
    text.push_str(rest);
    Some(text.trim().to_owned())
}

/// The index of the delimiter closing an already open one, past nested
/// pairs. Headings with a `\` never get here, so there are no escapes.
fn closing(text: &str, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0_usize;
    for (index, byte) in text.bytes().enumerate() {
        if byte == open {
            depth += 1;
        } else if byte == close {
            if depth == 0 {
                return Some(index);
            }
            depth -= 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{body_title, into_stored, title_drift};

    #[test]
    fn line_endings_take_the_stored_form() {
        assert_eq!(into_stored("a\r\nb\rc\n".to_owned()), "a\nb\nc\n");
        assert_eq!(into_stored("\u{feff}x\n\n".to_owned()), "\u{feff}x\n\n");
        for body in ["", "\r", "\r\n", "\r\r\n", "\n\r", "台\r\n灣\r\r末\r"] {
            let expected = body.replace("\r\n", "\n").replace('\r', "\n");
            assert_eq!(into_stored(body.to_owned()), expected);
        }
    }

    /// A heading or front matter past its cap is never compared or looked
    /// through, whatever it holds, which also bounds a hostile one.
    #[test]
    fn what_is_past_a_cap_gives_no_title() {
        let long = format!("# {}\n", "x".repeat(1025));
        assert_eq!(body_title(&long), None);
        assert_eq!(title_drift(&"x".repeat(1025), &long), None);
        assert_eq!(body_title(&format!("# {}\n", "[".repeat(1 << 20))), None);
        let front = format!("---\n{}title: T\n---\n", "key: v\n".repeat(300));
        assert_eq!(body_title(&front), None);
    }

    /// Each body and the title it gives; none wherever the parser cannot read
    /// the title for certain.
    const TITLE_CASES: &[(&str, Option<&str>)] = &[
        // Front matter first, then an H1 that is the first line of text.
        ("---\ntags: x\ntitle: \"Yaml\"\n---\n# H1\n", Some("Yaml")),
        (
            "---\ntags: x\n---\n\n# [Series](https://e.x/)：Part\n",
            Some("Series：Part"),
        ),
        // Anything before the H1 gives none, even when it is plainly no title.
        (
            "---\ntags: x\n---\n```\n# not a heading\n```\n# [Series](https://e.x/)：Part\n",
            None,
        ),
        ("## only h2\nplain\n", None),
        ("---\ntitle: Plain # note\n---\n", Some("Plain")),
        ("---\ntitle: \"Q # kept\" # c\n---\n", Some("Q # kept")),
        ("---\ntitle: |\n  block\n---\n# H\n", None),
        ("  #\tTitle ##\n", Some("Title")),
        ("# C#\n", Some("C#")),
        ("    # indented code\n#NoSpace\n", None),
        ("# [Title](https://e.x/a_(b))\n", Some("Title")),
        // An unclosed `---` is a rule, so no H1 comes first.
        ("---\n\n# Title\n", None),
        ("---\n# Real\ntitle: Fake\n", None),
        ("---\ntitle: \"Say \\\"Hi\\\"\"\n---\n", None),
        ("---\ntitle: 'Bob''s note'\n---\n", None),
        ("---\ntitle: # draft\n---\n# H1\n", None),
        ("---\ntitle:\n  next line\n---\n# H1\n", None),
        ("Setext\n===\n\n# Sub\n", None),
        ("\n\n# After blanks\n", Some("After blanks")),
        ("---\ntitle: Real\t# c\n---\n", Some("Real")),
        ("---\ntitle: \"Q\"\t# c\n---\n", Some("Q")),
        ("---\ntitle: C#\n---\n", Some("C#")),
        ("---\ntitle: \"A\" junk\n---\n", None),
        ("---\ntitle: \"A\"#x\n---\n", None),
        ("---\ntitle: [A, B]\n---\n", None),
        ("---\ntitle: {name: A}\n---\n", None),
        ("---\ntitle: &label A\n---\n", None),
        ("---\ntitle: *label\n---\n", None),
        ("---\ntitle: !tag A\n---\n", None),
        ("---\ntitle: a: b\n---\n", None),
        ("---\ntitle: \"   \"\n---\n", None),
        ("<!--\n# Hidden\n-->\n# Real\n", None),
        ("# `[Text](url)`\n", None),
        ("# \\[Draft] Title\n", None),
        ("---\n\"title\": Real\n---\n# Wrong\n", None),
        ("---\n  title: Real\n---\n# Wrong\n", None),
        ("---\ntitle : Real\n---\n# Wrong\n", None),
        ("---\ntitle: First\n  Second\n---\n", None),
        ("---\ntitle: Proposed\nother:\n\tbad: value\n---\n", None),
        ("---\ntitle: Proposed\nother:\n \tbad: value\n---\n", None),
        (
            "---\ntitle: Proposed\nother:\n  nested: value\n---\n",
            Some("Proposed"),
        ),
        ("---\ntitle: Real\ntags: x\n---\n# H1\n", Some("Real")),
        ("---\ntitleImage: x\n---\n# H1\n", Some("H1")),
        ("> > # Hidden\n\n# Real\n", None),
        ("- item\n  # Listed\n\n# Real\n", None),
        ("---\ntitle: First\n\n  Second\n---\n", None),
        ("---\ntitle: First\n# comment\n  Second\n---\n", None),
        (
            "---\ndescription: \"Start\ntitle: Wrong\nEnd\"\n---\n# Right\n",
            None,
        ),
        ("---\n{title: Right}\n---\n# Wrong\n", None),
        // An escaped quote does not close the scalar, which runs on.
        (
            "---\ndescription: \"a \\\" b\ntitle: Wrong\nend: c\"\n---\n# Right\n",
            None,
        ),
        (
            "---\ndescription: 'Bob''s\ntitle: Wrong\nend: c'\n---\n# Right\n",
            None,
        ),
        ("---\n  tags: x\ntitle: T\n---\n", None),
        ("---\ntitle: A\ntitle: B\n---\n", None),
        ("---\ntitle: null\n---\n# Right\n", None),
        ("---\ntitle: No\n---\n", None),
        ("---\ntitle: 01\n---\n", None),
        ("---\ntitle: 2026-10-08\n---\n", None),
        ("---\ntitle: ~\n---\n# Right\n", None),
        ("---\ntitle:My Note\n---\n# Right\n", None),
        ("---\ntitle: a:\tb\n---\n", None),
        ("---\ntitle: a:\n---\n", None),
        ("# [Title](<foo>bar)\n", None),
        ("# [Title](<foo)>)\n", None),
        (
            "---\ntags:\n- a\n  - b\nimage: https://e.x/a.png\ntitle: Kept\n---\n# H\n",
            Some("Kept"),
        ),
        ("---\ntitle: \"Quoted: yes\"\n---\n", Some("Quoted: yes")),
        ("# a <b>x</b>\n", None),
        // The indented line is the text of `description`, not a key.
        ("---\ndescription: |\n  title: x\n---\n# H1\n", Some("H1")),
        ("# [Real](not a url)\n", None),
        ("# **Bold** title\n", None),
        ("# a ~~b~~\n", None),
        ("# ==mark==\n", None),
        ("# &lt;b&gt;x\n", None),
        ("# [snake_case](https://e.x/a_b?c=1&d=2)\n", None),
        ("# [Plain](https://e.x/a_b?c=1&d=2)\n", Some("Plain")),
        (
            "---\ndescription: {\ntitle: Wrong,\nother: value}\n---\n# Right\n",
            None,
        ),
        ("---\ndescription: [\ntitle: Wrong]\n---\n# Right\n", None),
        (
            "---\ndescription: [\"]\"\ntitle: Wrong\n---\n# Right\n",
            None,
        ),
        (
            "---\ndescription: !!str a\ntitle: Wrong\n---\n# Right\n",
            None,
        ),
        (
            "---\ntags: [a, b]\nimage: {w: 1}\ntitle: Kept\n---\n",
            Some("Kept"),
        ),
        ("[ref]: /url \"start\n# Hidden\nend\"\n\n# Right\n", None),
        ("# [Title](foo\u{1}bar)\n", None),
        // Every key's value must end on its line in a form this reads.
        ("---\ntags: a: b\ntitle: Wrong\n---\n# Right\n", None),
        ("---\ntags: [a}\ntitle: Wrong\n---\n# Right\n", None),
        (
            "---\ndescription: \"ok\" junk\ntitle: Wrong\n---\n# Right\n",
            None,
        ),
        ("---\nkey: - a\ntitle: Wrong\n---\n", None),
        // Only what YAML lets follow a value may follow it.
        (
            "---\ndescription: \"ok\"\n  more\ntitle: Wrong\n---\n# Right\n",
            None,
        ),
        ("---\ntags: [a]\n  more\ntitle: Wrong\n---\n# Right\n", None),
        ("---\nkey: a\n  b: c\ntitle: Wrong\n---\n# Right\n", None),
        ("---\nkey: a\n- b\ntitle: Wrong\n---\n# Right\n", None),
        (
            "---\nkey: plain\n  folded on\ntitle: Kept\n---\n",
            Some("Kept"),
        ),
        (
            "---\nlist:\n- a\nmap:\n  b: c\ntitle: Kept\n---\n",
            Some("Kept"),
        ),
        ("---\ntags: # none\ntitle: Kept\n---\n", Some("Kept")),
        (
            "---\ndescription: >-\n  folded\ntitle: Kept\n---\n",
            Some("Kept"),
        ),
        (
            "---\nimage: \"a.png\" # c\nlinks: {a: [1, 2]}\ntitle: Kept\n---\n",
            Some("Kept"),
        ),
    ];

    #[test]
    fn only_yaml_and_markdown_it_reads_for_certain_give_a_title() {
        let wrong: Vec<_> = TITLE_CASES
            .iter()
            .map(|&(body, title)| (body, title, body_title(body)))
            .filter(|(_, title, got)| got.as_deref() != *title)
            .collect();
        assert!(wrong.is_empty(), "(body, expected, got): {wrong:#?}");
    }

    #[test]
    fn reference_links_and_images_never_recommend_a_rename() {
        for heading in [
            "[Title][ref]",
            "[Title][]",
            "[Title]",
            "[Draft] [Title](url)",
            "![Title](url)",
            "[Title](url) [ref]",
            "[Title [ref]](url)",
        ] {
            let body = format!(
                "# {heading}\n\n[ref]: https://example.com\n[Title]: https://example.com\n"
            );
            assert_eq!(title_drift("Title", &body), None, "{heading}");
        }
        assert_eq!(
            title_drift("Old", "# [Title](url)\n").as_deref(),
            Some("Title")
        );
    }

    #[test]
    fn drift_is_reported_only_when_the_titles_differ() {
        assert_eq!(title_drift("Old", "# New\n").as_deref(), Some("New"));
        assert_eq!(title_drift("Same", "# Same\n"), None);
        assert_eq!(title_drift("Alpha Beta", "# Alpha  Beta\n"), None);
        assert_eq!(title_drift("Alpha  Beta", "# Alpha\tBeta\n"), None);
        assert_eq!(
            title_drift("Old", "# Alpha  Beta\n").as_deref(),
            Some("Alpha  Beta")
        );
        assert_eq!(title_drift("Untitled", "no heading\n"), None);
        assert_eq!(title_drift("  Alpha\u{2003}Beta  ", "# Alpha Beta\n"), None);
        assert_eq!(
            title_drift(&"x ".repeat(1 << 20), "# x\n").as_deref(),
            Some("x")
        );
    }
}
