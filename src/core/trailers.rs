//! Trailer keys that record the cross-repo commit mapping.
//!
//! - Public commits exported from the monorepo carry `Monosplice-Source: <mono-sha>`.
//! - Monorepo commits imported from a public repo carry `Monosplice-Origin: <pub-sha>`,
//!   marking that pub commit as reflected in the monorepo.
//!
//! Import skips pub commits carrying Monosplice-Source (our own exports). Export
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

/// Is this line one of monosplice's own sync trailers? Keys compare case-insensitively, the
/// way git's trailer parsing (which the sync view reads through) compares them.
fn is_sync_trailer_line(line: &str) -> bool {
    let Some(idx) = line.find(':') else {
        return false;
    };
    let key = line[..idx].trim_end_matches(is_space);
    key.eq_ignore_ascii_case(SOURCE_TRAILER) || key.eq_ignore_ascii_case(ORIGIN_TRAILER)
}

/// Remove every `Monosplice-Source` / `Monosplice-Origin` line from the final paragraph of a
/// message, dropping the paragraph if nothing else was in it.
///
/// A sync trailer states a fact about the one hop that wrote it: this commit reflects that
/// commit, in the repository on the other side of *that* boundary. Carried into the next
/// repository it reads as that repository's own claim, so every replay strips them before
/// appending its own. The whole final paragraph is searched, not just a strict trailer block:
/// git also reads trailers from a paragraph that mixes them with prose once a git-generated
/// trailer such as `Signed-off-by` is present.
///
/// A message with nothing to strip is returned byte-for-byte, so a single-hop replay produces
/// exactly the message it always did.
pub fn strip_sync_trailers(message: &str) -> String {
    let normalized = message.replace("\r\n", "\n");
    let body = trim_end_js(&normalized);
    // The subject is never a trailer; a one-paragraph message has nothing to strip.
    let Some(split) = body.rfind("\n\n") else {
        return message.to_string();
    };
    let last = &body[split + 2..];
    let lines: Vec<&str> = last.split('\n').collect();
    let kept: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !is_sync_trailer_line(line))
        .collect();
    if kept.len() == lines.len() {
        return message.to_string();
    }
    let head = trim_end_js(&body[..split]);
    if kept.iter().all(|line| trim_js(line).is_empty()) {
        return format!("{head}\n");
    }
    format!("{head}\n\n{}\n", kept.join("\n"))
}

/// Which of monosplice's sync trailers a commit carries as its *own*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncTrailer {
    Source(String),
    Origin(String),
}

impl SyncTrailer {
    /// Parse one `Key: value` line as git's `%(trailers)` prints it. Anything that is not a
    /// sync trailer, or has no value, is `None`.
    pub fn parse(line: &str) -> Option<SyncTrailer> {
        let idx = line.find(':')?;
        let key = trim_js(&line[..idx]);
        let value = trim_js(&line[idx + 1..]).to_string();
        if value.is_empty() {
            return None;
        }
        if key.eq_ignore_ascii_case(SOURCE_TRAILER) {
            Some(SyncTrailer::Source(value))
        } else if key.eq_ignore_ascii_case(ORIGIN_TRAILER) {
            Some(SyncTrailer::Origin(value))
        } else {
            None
        }
    }
}

/// The sync trailer the hop that created this commit wrote: the *last* `Monosplice-Source` or
/// `Monosplice-Origin` in the final trailer block.
///
/// Every replay appends its own trailer after whatever the message already carried, so when a
/// message holds more than one, the earlier ones were forwarded from a previous hop (monosplice
/// 1.0.0 copied them verbatim) and describe some other repository's boundary. Only the last is
/// this commit's claim. Published history cannot be rewritten, so this is how commits that
/// already carry forwarded trailers stay readable.
pub fn writer_trailer(message: &str) -> Option<SyncTrailer> {
    let blocks = paragraphs(message);
    if blocks.len() < 2 {
        return None;
    }
    let last = blocks.last()?;
    if !is_trailer_block(last) {
        return None;
    }
    last.split('\n').filter_map(SyncTrailer::parse).next_back()
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
    fn strip_leaves_a_message_without_sync_trailers_byte_for_byte() {
        for msg in [
            "feat: x",
            "feat: x\n",
            "feat: x\r\n\r\nbody\r\n",
            "feat: x\n\n\n\nbody\n\nSigned-off-by: A <a@b.c>\n\n\n",
            "Monosplice-Source: only a subject",
        ] {
            assert_eq!(strip_sync_trailers(msg), msg);
        }
    }

    #[test]
    fn strip_removes_forwarded_sync_trailers_and_keeps_the_rest() {
        assert_eq!(
            strip_sync_trailers("outer: patch lib\n\nMonosplice-Source: abc\n"),
            "outer: patch lib\n"
        );
        assert_eq!(
            strip_sync_trailers(
                "fix: y\n\nBody.\n\nSigned-off-by: A <a@b.c>\nMonosplice-Origin: l1\nmonosplice-source: m1\nCo-authored-by: B <b@c.d>\n"
            ),
            "fix: y\n\nBody.\n\nSigned-off-by: A <a@b.c>\nCo-authored-by: B <b@c.d>\n"
        );
        // A paragraph git would read as trailers because of Signed-off-by, prose included.
        assert_eq!(
            strip_sync_trailers("s\n\nSigned-off-by: A <a@b.c>\nMonosplice-Source: m\nprose\n"),
            "s\n\nSigned-off-by: A <a@b.c>\nprose\n"
        );
        // Only the final paragraph holds trailers; an earlier one is body text.
        let body_mention = "s\n\nMonosplice-Source: in the body\n\nTicket: 7\n";
        assert_eq!(strip_sync_trailers(body_mention), body_mention);
    }

    #[test]
    fn strip_then_append_leaves_exactly_one_sync_trailer() {
        let forwarded = "outer: patch lib\n\nMonosplice-Source: outer\n";
        let out = append_trailer(&strip_sync_trailers(forwarded), SOURCE_TRAILER, "middle");
        assert_eq!(out, "outer: patch lib\n\nMonosplice-Source: middle\n");

        let doubled = "leaf: add b\n\nMonosplice-Origin: leaf\nMonosplice-Origin: middle\n";
        let out = append_trailer(&strip_sync_trailers(doubled), ORIGIN_TRAILER, "outer");
        assert_eq!(out, "leaf: add b\n\nMonosplice-Origin: outer\n");
    }

    #[test]
    fn the_writer_trailer_is_the_last_sync_trailer() {
        assert_eq!(writer_trailer("s"), None);
        assert_eq!(writer_trailer("s\n\nSigned-off-by: A <a@b.c>\n"), None);
        assert_eq!(
            writer_trailer("s\n\nMonosplice-Origin: l\nMonosplice-Origin: m\n"),
            Some(SyncTrailer::Origin("m".to_string()))
        );
        assert_eq!(
            writer_trailer(
                "s\n\nMonosplice-Origin: u\nMonosplice-Source: x\nSigned-off-by: A <a@b.c>\n"
            ),
            Some(SyncTrailer::Source("x".to_string()))
        );
        // The same prose rule as get_trailer: not a trailer block, no claim.
        assert_eq!(writer_trailer("s\n\nMonosplice-Origin: u\nprose\n"), None);
    }

    #[test]
    fn sync_trailer_lines_parse_case_insensitively() {
        assert_eq!(
            SyncTrailer::parse("monosplice-source: abc"),
            Some(SyncTrailer::Source("abc".to_string()))
        );
        assert_eq!(
            SyncTrailer::parse("Monosplice-Origin:  def "),
            Some(SyncTrailer::Origin("def".to_string()))
        );
        assert_eq!(SyncTrailer::parse("Signed-off-by: x"), None);
        assert_eq!(SyncTrailer::parse("Monosplice-Source: "), None);
    }

    #[test]
    fn trailing_whitespace_is_trimmed_before_appending() {
        assert_eq!(
            append_trailer("feat: x\n\n\n", SOURCE_TRAILER, "abc"),
            "feat: x\n\nMonosplice-Source: abc\n"
        );
    }
}
