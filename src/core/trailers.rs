//! Trailer keys that record the cross-repo commit mapping.
//!
//! - Public commits exported from the monorepo carry `Monosplice-Source: <mono-sha> <id>`, or
//!   the legacy `Monosplice-Source: <mono-sha>` when the monorepo has no `id`.
//! - Monorepo commits imported from a public repo carry `Monosplice-Origin: <pub-sha>`,
//!   marking that pub commit as reflected in the monorepo.
//!
//! Import skips pub commits carrying *our* Monosplice-Source (our own exports); which
//! trailers are ours is decided in `sync_view`, since several monorepos may publish to one
//! public repo. Export
//! relies on tree equality, not trailers: pure imports are no-ops against the pub
//! tip and get dropped, while conflicted imports (merges of mono + pub edits)
//! export their resolution. Together these prevent ping-pong without ever losing
//! merge resolutions.

pub const SOURCE_TRAILER: &str = "Monosplice-Source";
pub const ORIGIN_TRAILER: &str = "Monosplice-Origin";

/// A trailer line: `^[A-Za-z0-9-]+:\s.+$` — a key, a colon, one whitespace character,
/// then at least one more character that is not a line break.
fn is_trailer_line(line: &str) -> bool {
    let Some(idx) = line.find(':') else {
        return false;
    };
    let key = &line[..idx];
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return false;
    }
    let mut rest = line[idx + 1..].chars();
    match rest.next() {
        Some(c) if is_space(c) => {}
        _ => return false,
    }
    let value: String = rest.collect();
    !value.is_empty() && !value.chars().any(is_line_terminator)
}

/// JavaScript's `\s` (WhiteSpace ∪ LineTerminator), which is what the TS regex and
/// `trim`/`trimEnd` used.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{000b}' | '\u{000c}')
        || matches!(c, '\u{00a0}' | '\u{1680}' | '\u{2028}' | '\u{2029}')
        || matches!(c, '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
        || ('\u{2000}'..='\u{200a}').contains(&c)
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn trim_end_js(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

fn trim_js(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Split a commit message into paragraphs (blocks separated by blank lines) — the TS
/// `message.replace(/\r\n/g, '\n').trimEnd().split(/\n{2,}/)`.
fn paragraphs(message: &str) -> Vec<String> {
    let normalized = message.replace("\r\n", "\n");
    let body = trim_end_js(&normalized);
    let mut blocks = Vec::new();
    let mut current = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' && chars.peek() == Some(&'\n') {
            while chars.peek() == Some(&'\n') {
                chars.next();
            }
            blocks.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    blocks.push(current);
    blocks
}

fn is_trailer_block(block: &str) -> bool {
    block.split('\n').all(is_trailer_line)
}

/// Read a trailer value from a commit message. Mirrors git's semantics closely
/// enough for monosplice's own trailers: only the final paragraph counts, and only
/// when that whole paragraph is a trailer block.
pub fn get_trailer(message: &str, key: &str) -> Option<String> {
    let blocks = paragraphs(message);
    // A message that is only one paragraph has no trailer block (it's the subject).
    if blocks.len() < 2 {
        return None;
    }
    let last = blocks.last()?;
    if !is_trailer_block(last) {
        return None;
    }
    for line in last.split('\n') {
        if let Some(idx) = line.find(':') {
            if &line[..idx] == key {
                return Some(trim_js(&line[idx + 1..]).to_string());
            }
        }
    }
    None
}

/// Append a trailer to a commit message, extending an existing trailer block if
/// the message ends with one, otherwise starting a new block.
pub fn append_trailer(message: &str, key: &str, value: &str) -> String {
    let normalized = message.replace("\r\n", "\n");
    let body = trim_end_js(&normalized);
    if body.is_empty() {
        return format!("{key}: {value}\n");
    }
    let blocks = paragraphs(body);
    let last = blocks.last().map(String::as_str).unwrap_or("");
    if blocks.len() > 1 && is_trailer_block(last) {
        return format!("{body}\n{key}: {value}\n");
    }
    format!("{body}\n\n{key}: {value}\n")
}

/// A `Monosplice-Source` value: the monorepo commit, and the id of the monorepo that
/// published it. Legacy exports, and monorepos with no `id`, carry the sha alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    pub mono_sha: String,
    pub id: Option<String>,
}

/// Read a `Monosplice-Source` value: `<sha>`, or `<sha> <id>`.
pub fn parse_source(value: &str) -> SourceRef {
    let mut words = value.split_whitespace();
    SourceRef {
        mono_sha: words.next().unwrap_or("").to_string(),
        id: words.next().map(str::to_string),
    }
}

/// The value an export writes: tagged with the monorepo's id when it has one.
pub fn format_source(mono_sha: &str, id: Option<&str>) -> String {
    match id {
        Some(id) => format!("{mono_sha} {id}"),
        None => mono_sha.to_string(),
    }
}

fn is_key_line(line: &str, key: &str) -> bool {
    is_trailer_line(line)
        && line
            .split(':')
            .next()
            .is_some_and(|k| k.eq_ignore_ascii_case(key))
}

/// Drop every `key:` line (key compared ASCII case-insensitively) from the message's trailer
/// block — its final paragraph, and only when that whole paragraph is trailers, the same test
/// [`get_trailer`] uses. Everything before it is left as it was; a message with nothing to
/// remove comes back byte-identical.
pub fn remove_trailer(message: &str, key: &str) -> String {
    let normalized = message.replace("\r\n", "\n");
    let body = trim_end_js(&normalized);
    let Some(run_start) = body.rfind("\n\n") else {
        return message.to_string();
    };
    let last = &body[run_start..].trim_start_matches('\n');
    if !is_trailer_block(last) || !last.split('\n').any(|line| is_key_line(line, key)) {
        return message.to_string();
    }
    let head = body[..run_start].trim_end_matches('\n');
    let kept: Vec<&str> = last
        .split('\n')
        .filter(|line| !is_key_line(line, key))
        .collect();
    let gap = &body[head.len()..body.len() - last.len()];
    if kept.is_empty() {
        return format!("{head}\n");
    }
    format!("{head}{gap}{}\n", kept.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_a_trailer_as_a_new_block_after_a_plain_message() {
        assert_eq!(
            append_trailer("feat: add thing", SOURCE_TRAILER, "abc123"),
            "feat: add thing\n\nMonosplice-Source: abc123\n"
        );
    }

    #[test]
    fn appends_into_an_existing_trailer_block() {
        let msg = "feat: add thing\n\nLonger body here.\n\nSigned-off-by: Someone <s@x.y>\n";
        assert_eq!(
            append_trailer(msg, ORIGIN_TRAILER, "def456"),
            "feat: add thing\n\nLonger body here.\n\nSigned-off-by: Someone <s@x.y>\nMonosplice-Origin: def456\n"
        );
    }

    #[test]
    fn round_trips_get_trailer_reads_what_append_trailer_wrote() {
        let out = append_trailer("fix: bug\n\nBody paragraph.", SOURCE_TRAILER, "cafe01");
        assert_eq!(get_trailer(&out, SOURCE_TRAILER).as_deref(), Some("cafe01"));
        assert_eq!(get_trailer(&out, ORIGIN_TRAILER), None);
    }

    #[test]
    fn does_not_read_a_subject_line_as_a_trailer() {
        assert_eq!(
            get_trailer("Monosplice-Source: not-really", SOURCE_TRAILER),
            None
        );
    }

    #[test]
    fn only_reads_the_final_block() {
        let msg = "subj\n\nMonosplice-Source: old\n\nActual final paragraph of prose.";
        assert_eq!(get_trailer(msg, SOURCE_TRAILER), None);
    }

    #[test]
    fn handles_multi_trailer_final_blocks() {
        let msg = "subj\n\nMonosplice-Source: aaa\nMonosplice-Origin: bbb\n";
        assert_eq!(get_trailer(msg, SOURCE_TRAILER).as_deref(), Some("aaa"));
        assert_eq!(get_trailer(msg, ORIGIN_TRAILER).as_deref(), Some("bbb"));
    }

    // Ports of the TS semantics the vitest suite did not spell out but the
    // exporter/importer depend on.

    #[test]
    fn normalizes_crlf_before_reading_and_writing() {
        let msg = "subj\r\n\r\nMonosplice-Source: aaa\r\n";
        assert_eq!(get_trailer(msg, SOURCE_TRAILER).as_deref(), Some("aaa"));
        assert_eq!(
            append_trailer("feat: x\r\n", ORIGIN_TRAILER, "bbb"),
            "feat: x\n\nMonosplice-Origin: bbb\n"
        );
    }

    #[test]
    fn an_empty_message_becomes_a_bare_trailer() {
        assert_eq!(
            append_trailer("", SOURCE_TRAILER, "abc"),
            "Monosplice-Source: abc\n"
        );
        assert_eq!(
            append_trailer("   \n\n ", SOURCE_TRAILER, "abc"),
            "Monosplice-Source: abc\n"
        );
    }

    #[test]
    fn a_final_block_with_a_prose_line_is_not_a_trailer_block() {
        let msg = "subj\n\nMonosplice-Source: aaa\nnot a trailer line\n";
        assert_eq!(get_trailer(msg, SOURCE_TRAILER), None);
        // ...and appending starts a fresh block rather than extending it.
        assert_eq!(
            append_trailer(msg, ORIGIN_TRAILER, "bbb"),
            "subj\n\nMonosplice-Source: aaa\nnot a trailer line\n\nMonosplice-Origin: bbb\n"
        );
    }

    #[test]
    fn trailer_keys_must_match_exactly_and_values_are_trimmed() {
        let msg = "subj\n\nMonosplice-Source:   aaa  \nX-Monosplice-Source: bbb\n";
        assert_eq!(get_trailer(msg, SOURCE_TRAILER).as_deref(), Some("aaa"));
        // The `X-` prefixed line has a key that is not `Monosplice-Source`.
        assert_eq!(get_trailer(msg, "Monosplice-Sourc"), None);
    }

    #[test]
    fn a_trailer_line_needs_a_space_and_a_value() {
        // "Key:value" (no whitespace) and "Key: " (no value) are not trailer lines,
        // so the paragraph is not a trailer block.
        assert_eq!(
            get_trailer("subj\n\nMonosplice-Source:aaa", SOURCE_TRAILER),
            None
        );
        assert_eq!(
            get_trailer("subj\n\nMonosplice-Source: ", SOURCE_TRAILER),
            None
        );
    }

    #[test]
    fn paragraphs_split_on_runs_of_two_or_more_newlines() {
        let msg = "subj\n\n\n\nMonosplice-Source: aaa";
        assert_eq!(get_trailer(msg, SOURCE_TRAILER).as_deref(), Some("aaa"));
    }

    #[test]
    fn a_source_value_tagged_with_an_id_parses_into_sha_and_id() {
        assert_eq!(
            parse_source("abc123 pearly"),
            SourceRef {
                mono_sha: "abc123".to_string(),
                id: Some("pearly".to_string()),
            }
        );
        assert_eq!(format_source("abc123", Some("pearly")), "abc123 pearly");
        let round = append_trailer("x", SOURCE_TRAILER, &format_source("abc", Some("b")));
        let value = get_trailer(&round, SOURCE_TRAILER).expect("trailer");
        assert_eq!(parse_source(&value).id.as_deref(), Some("b"));
    }

    #[test]
    fn a_legacy_source_value_has_no_id() {
        assert_eq!(
            parse_source("abc123"),
            SourceRef {
                mono_sha: "abc123".to_string(),
                id: None,
            }
        );
        assert_eq!(format_source("abc123", None), "abc123");
        // Stray whitespace is not an id.
        assert_eq!(parse_source("  abc123  ").id, None);
        assert_eq!(parse_source("  abc123  ").mono_sha, "abc123");
    }

    #[test]
    fn removing_source_lines_keeps_every_other_trailer() {
        let msg = "feat: x\n\nBody.\n\nSigned-off-by: S <s@x.y>\nMonosplice-Source: aaa\nMonosplice-Origin: bbb\nMonosplice-Source: ccc other\n";
        assert_eq!(
            remove_trailer(msg, SOURCE_TRAILER),
            "feat: x\n\nBody.\n\nSigned-off-by: S <s@x.y>\nMonosplice-Origin: bbb\n"
        );
    }

    #[test]
    fn removing_the_only_trailer_drops_the_empty_block() {
        assert_eq!(
            remove_trailer("feat: x\n\nMonosplice-Source: aaa\n", SOURCE_TRAILER),
            "feat: x\n"
        );
    }

    #[test]
    fn a_message_without_the_trailer_is_left_byte_identical() {
        let msg = "feat: x\n\n\n\nodd spacing kept\n";
        assert_eq!(remove_trailer(msg, SOURCE_TRAILER), msg);
    }

    #[test]
    fn a_source_line_in_a_body_paragraph_is_kept_byte_identical() {
        let msg = "feat: x\n\n\n\nMonosplice-Source: quoted in prose\nmore prose\n\nSigned-off-by: S <s@x.y>\nMonosplice-Source: aaa\n";
        assert_eq!(
            remove_trailer(msg, SOURCE_TRAILER),
            "feat: x\n\n\n\nMonosplice-Source: quoted in prose\nmore prose\n\nSigned-off-by: S <s@x.y>\n"
        );
        // A final paragraph that is not a trailer block is not touched at all.
        let prose = "feat: x\n\nMonosplice-Source: aaa\nnot a trailer\n";
        assert_eq!(remove_trailer(prose, SOURCE_TRAILER), prose);
    }

    #[test]
    fn a_source_trailer_key_is_matched_case_insensitively() {
        assert_eq!(
            remove_trailer(
                "feat: x\n\nmonosplice-source: aaa\nMONOSPLICE-SOURCE: bbb b\nOther: c\n",
                SOURCE_TRAILER
            ),
            "feat: x\n\nOther: c\n"
        );
    }

    #[test]
    fn trailing_whitespace_is_trimmed_before_appending() {
        assert_eq!(
            append_trailer("feat: x\n\n\n", SOURCE_TRAILER, "abc"),
            "feat: x\n\nMonosplice-Source: abc\n"
        );
    }
}
