//! Port of src/core (see docs/rust-port.md).
//!
//! Turning a git URL and a folder into a subrepo entry, writing it into `monosplice.toml`,
//! and refusing to when the file is not something a line-wise edit may safely rewrite.
//! Everything here is pure text or read-only git, so `attach` can run every check before it
//! writes a byte.
//!
//! The module is named for the retired `vendor` command it was extracted from; `attach`
//! absorbed that command and is now its only caller.

use std::io;
use std::path::Path;

use crate::config::{load_project, Project, ResolvedSubrepo};
use crate::core::git::{git, git_ok, rev_parse};
use crate::core::paths::normalize_subrepo_path;
use crate::core::sync_view::pull_source;

/// A subrepo entry as it exists before the config knows about it.
#[derive(Debug, Clone)]
pub struct VendorEntry {
    pub name: String,
    pub path: String,
    pub remote: String,
    pub branch: String,
    /// Set by `--fork`: `remote` is then the fork and this is where the tree comes from.
    pub upstream: Option<String>,
    /// Branch pushed on the fork. Omitted from the rendered entry when it equals `branch`.
    pub push_branch: Option<String>,
}

impl From<&ResolvedSubrepo> for VendorEntry {
    fn from(s: &ResolvedSubrepo) -> Self {
        VendorEntry {
            name: s.name.clone(),
            path: s.path.clone(),
            remote: s.remote.clone(),
            branch: s.branch.clone(),
            upstream: s.upstream.clone(),
            push_branch: Some(s.push_branch.clone()),
        }
    }
}

/// TOML basic string. Config files are hand-edited, so keep them readable: basic strings are
/// what the docs show and what every key in the template uses.
fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Last segment of a `/`-separated path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The entry to write into the config, in the same style the README documents. `name`,
/// `branch` and `push-branch` are omitted when they equal what the loader would default to
/// anyway. No trailing newline — the inserter owns the file's line endings.
pub fn render_subrepo_entry(e: &VendorEntry) -> String {
    let mut lines = vec!["[[subrepos]]".to_string()];
    if e.name != basename(&e.path) {
        lines.push(format!("name = {}", quote(&e.name)));
    }
    lines.push(format!("path = {}", quote(&e.path)));
    lines.push(format!("remote = {}", quote(&e.remote)));
    if e.branch != "main" {
        lines.push(format!("branch = {}", quote(&e.branch)));
    }
    if let Some(upstream) = &e.upstream {
        lines.push(format!("upstream = {}", quote(upstream)));
        if let Some(push_branch) = &e.push_branch {
            if push_branch != &e.branch {
                lines.push(format!("push-branch = {}", quote(push_branch)));
            }
        }
    }
    lines.join("\n")
}

/// Append the entry at the end of the file. An array of tables can only ever grow at the
/// bottom — there is no "insert into the array" for TOML, which is exactly why this half of
/// the bargain can no longer fail; the reload check below is what still can.
pub fn insert_subrepo_entry(source: &str, entry_block: &str) -> String {
    let trimmed = source.trim_end_matches(['\n', '\r']);
    if trimmed.trim().is_empty() {
        format!("{entry_block}\n")
    } else {
        format!("{trimmed}\n\n{entry_block}\n")
    }
}

/// A `key = value` line the remover was able (or unable) to read.
enum Field {
    Missing,
    /// Present but not a plain single-line string: refuse rather than guess.
    Unreadable,
    Value(String),
}

/// Read `key` out of a `[[subrepos]]` block's body lines. Only a plain, single-line TOML
/// string counts — a multiline string, a computed-looking value or a non-string is a shape
/// this module may not rewrite.
fn read_field(body: &[&str], key: &str) -> Field {
    for line in body {
        let text = line.trim();
        if text.is_empty() || text.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = text.split_once('=') else {
            continue;
        };
        if raw_key.trim().trim_matches(|c| c == '"' || c == '\'') != key {
            continue;
        }
        let value = raw_value.trim();
        if value.starts_with("\"\"\"") || value.starts_with("'''") {
            return Field::Unreadable;
        }
        if !(value.starts_with('"') || value.starts_with('\'')) {
            return Field::Unreadable;
        }
        let parsed: Result<toml::Value, _> = format!("v = {value}").parse();
        return match parsed
            .ok()
            .and_then(|v| v.get("v").and_then(toml::Value::as_str).map(str::to_string))
        {
            Some(s) => Field::Value(s),
            None => Field::Unreadable,
        };
    }
    Field::Missing
}

/// The name the loader would give this entry, or `None` when it cannot be read literally.
fn entry_name(body: &[&str]) -> Option<String> {
    match read_field(body, "name") {
        Field::Value(name) => return Some(name),
        Field::Unreadable => return None,
        Field::Missing => {}
    }
    match read_field(body, "path") {
        Field::Value(path) => normalize_subrepo_path(&path)
            .ok()
            .map(|p| basename(&p).to_string()),
        _ => None,
    }
}

/// The reverse of `insert_subrepo_entry`: delete the block the loader would name `name`, or
/// return `None` when the file does not spell it out plainly enough to edit. Deliberately as
/// naive as its counterpart — a block runs from its `[[subrepos]]` header to the next header
/// or EOF, its identity must be readable from a plain string, and exactly one may match.
pub fn remove_subrepo_entry(source: &str, name: &str) -> Option<String> {
    let mut lines: Vec<&str> = source.split('\n').collect();
    // `split` leaves an empty tail for the final newline; the rejoin re-adds it.
    if source.ends_with('\n') {
        lines.pop();
    }

    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "[[subrepos]]" {
            let start = i;
            let mut end = i + 1;
            while end < lines.len() && !lines[end].trim_start().starts_with('[') {
                end += 1;
            }
            blocks.push((start, end));
            i = end;
        } else {
            i += 1;
        }
    }

    let mut hits: Vec<(usize, usize)> = Vec::new();
    for (start, end) in blocks {
        // A block monosplice cannot read might BE the one to delete; refuse rather than guess.
        let resolved = entry_name(&lines[start + 1..end])?;
        if resolved == name {
            hits.push((start, end));
        }
    }
    if hits.len() != 1 {
        return None;
    }

    // The block already owns the blank line that separated it from the next one (its range
    // runs to the following header), so cutting it leaves the file tidy — except at a seam
    // where a blank line now sits on both sides, and at EOF, where the trailing blanks go.
    let (start, end) = hits[0];
    lines.drain(start..end);
    while start > 0
        && start < lines.len()
        && lines[start - 1].trim().is_empty()
        && lines[start].trim().is_empty()
    {
        lines.remove(start);
    }

    let kept = lines.join("\n");
    if kept.trim().is_empty() {
        return Some(String::new());
    }
    let mut out = kept.trim_end_matches(['\n', '\r']).to_string();
    out.push('\n');
    Some(out)
}

/// How a command tells the user to pick a different name or a different directory.
pub struct SlotHints {
    /// Clause, no trailing period: "Vendor it under another name with `--name <name>`".
    pub rename: String,
    /// Full sentence: "Pick another directory with `--path <dir>`."
    pub relocate: String,
}

/// Why this name/path pair cannot become a new subrepo, or `None` when the slot is free. Both
/// halves must be free, and the path may not nest inside (or contain) a configured subrepo —
/// the config loader would reject the file monosplice is about to write anyway.
pub fn check_free_slot(
    subrepos: &[ResolvedSubrepo],
    entry: &VendorEntry,
    hints: &SlotHints,
) -> Option<String> {
    for s in subrepos {
        if s.name == entry.name {
            return Some(format!(
                "A subrepo named {} is already configured ({}/ tracking {}).\nNothing was changed. {}, or run `monosplice pull {}` if this is the one you meant.",
                entry.name, s.path, s.remote, hints.rename, s.name,
            ));
        }
        if s.path == entry.path {
            return Some(format!(
                "{} is already configured as subrepo {}.\nNothing was changed. {}",
                entry.path, s.name, hints.relocate,
            ));
        }
        if s.path.starts_with(&format!("{}/", entry.path))
            || entry.path.starts_with(&format!("{}/", s.path))
        {
            return Some(format!(
                "subrepo paths may not nest: {} and {} (subrepo {}) would sit inside one another.\nNothing was changed. {}",
                entry.path, s.path, s.name, hints.relocate,
            ));
        }
    }
    None
}

/// The config could not be edited safely. The file is back to its original bytes.
pub struct ConfigWriteFailure {
    /// The rendered entry, for the user to paste in by hand.
    pub snippet: String,
    /// Why monosplice will not touch the file.
    pub reason: String,
}

/// Why the config monosplice just wrote cannot be trusted, or `None` when it checks out.
fn reloaded_mismatch(root: &Path, entry: &ResolvedSubrepo) -> Option<String> {
    let reloaded = match load_project(root) {
        Ok(Some(project)) => project,
        Ok(None) => {
            return Some("the config file vanished while monosplice was writing it".to_string())
        }
        Err(err) => return Some(format!("the rewritten config does not load:\n{err}")),
    };
    let Some(found) = reloaded.subrepos.iter().find(|s| s.name == entry.name) else {
        return Some(format!(
            "the rewritten config has no subrepo named {}",
            entry.name
        ));
    };
    if found.path != entry.path
        || found.remote != entry.remote
        || found.branch != entry.branch
        || found.upstream != entry.upstream
        || found.push_branch != entry.push_branch
    {
        return Some(format!(
            "the rewritten config resolves {} to {}/ tracking {} ({}), not what monosplice wrote",
            entry.name,
            found.path,
            pull_source(found),
            found.branch,
        ));
    }
    None
}

/// Append the entry textually, then prove it by reloading the config through the real loader.
/// If the reload disagrees the original bytes go back and the caller hands the user the
/// snippet — a half-rewritten config file is far worse than one the user pastes into
/// themselves.
pub fn write_config_entry(
    project: &Project,
    entry: &ResolvedSubrepo,
) -> io::Result<Option<ConfigWriteFailure>> {
    let snippet = render_subrepo_entry(&VendorEntry::from(entry));
    let original = std::fs::read(&project.config_path)?;
    let updated = insert_subrepo_entry(&String::from_utf8_lossy(&original), &snippet);

    std::fs::write(&project.config_path, updated)?;
    if let Some(reason) = reloaded_mismatch(&project.root, entry) {
        std::fs::write(&project.config_path, &original)?;
        return Ok(Some(ConfigWriteFailure { snippet, reason }));
    }
    Ok(None)
}

/// The config could not have an entry removed from it. The file is back to its original bytes.
pub struct ConfigRemoveFailure {
    pub reason: String,
}

/// Why the config monosplice just trimmed cannot be trusted, or `None` when it checks out.
fn removed_mismatch(project: &Project, entry: &ResolvedSubrepo) -> Option<String> {
    let reloaded = match load_project(&project.root) {
        Ok(Some(project)) => project,
        Ok(None) => {
            return Some("the config file vanished while monosplice was writing it".to_string())
        }
        Err(err) => return Some(format!("the rewritten config does not load:\n{err}")),
    };
    if reloaded.subrepos.iter().any(|s| s.name == entry.name) {
        return Some(format!(
            "the rewritten config still has a subrepo named {}",
            entry.name
        ));
    }
    let expected: Vec<&ResolvedSubrepo> = project
        .subrepos
        .iter()
        .filter(|s| s.name != entry.name)
        .collect();
    if reloaded.subrepos.len() != expected.len() {
        return Some(format!(
            "the rewritten config resolves to {} subrepo(s) where {} were expected",
            reloaded.subrepos.len(),
            expected.len()
        ));
    }
    for (want, got) in expected.iter().zip(reloaded.subrepos.iter()) {
        if got.name != want.name
            || got.path != want.path
            || got.remote != want.remote
            || got.branch != want.branch
            || got.upstream != want.upstream
            || got.push_branch != want.push_branch
        {
            return Some(format!(
                "the rewritten config changed subrepo {}, which monosplice was not asked to touch",
                want.name
            ));
        }
    }
    None
}

/// Delete the entry textually, then prove it by reloading the config through the real loader:
/// the named subrepo must be gone and every other one must resolve exactly as it did before.
/// If either half fails the original bytes go back and the caller tells the user what to
/// delete by hand — the same bargain `write_config_entry` makes in the other direction.
pub fn remove_config_entry(
    project: &Project,
    entry: &ResolvedSubrepo,
) -> io::Result<Option<ConfigRemoveFailure>> {
    let original = std::fs::read(&project.config_path)?;
    let Some(updated) = remove_subrepo_entry(&String::from_utf8_lossy(&original), &entry.name)
    else {
        return Ok(Some(ConfigRemoveFailure {
            reason: format!(
                "no plain [[subrepos]] entry for {} that a text edit can safely remove",
                entry.name
            ),
        }));
    };

    std::fs::write(&project.config_path, updated)?;
    if let Some(reason) = removed_mismatch(project, entry) {
        std::fs::write(&project.config_path, &original)?;
        return Ok(Some(ConfigRemoveFailure { reason }));
    }
    Ok(None)
}

/// What to print when monosplice will not delete the entry itself: the removal is a two-line
/// instruction, so it goes to stdout and the error names what to run once it is done.
/// Returns `(log, error)`.
pub fn delete_it_yourself(
    config_path: &Path,
    entry: &ResolvedSubrepo,
    failure: &ConfigRemoveFailure,
) -> (String, String) {
    let config_path = config_path.display();
    (
        format!(
            "Delete the [[subrepos]] entry for {} ({}/ tracking {}) from {config_path}.\n",
            entry.name,
            entry.path,
            pull_source(entry),
        ),
        format!(
            "monosplice cannot safely edit {config_path}: {}.\nNothing was changed — the config is untouched and no commit was made. Delete the entry described above by hand and commit it; {}/ and its history stay exactly as they are either way.",
            failure.reason, entry.path,
        ),
    )
}

/// What to print when monosplice will not edit the config: the entry goes to stdout so it can
/// be piped or copy-pasted, and the error names the command to run once it is pasted in.
/// Returns `(log, error)`.
pub fn paste_it_yourself(
    config_path: &Path,
    failure: &ConfigWriteFailure,
    next_command: &str,
) -> (String, String) {
    let config_path = config_path.display();
    (
        format!(
            "Add this [[subrepos]] entry to {config_path}:\n\n{}\n",
            failure.snippet
        ),
        format!(
            "monosplice cannot safely edit {config_path}: {}.\nNothing was changed — the config is untouched and no commit was made. Paste the entry printed above into your config, then run:\n  {next_command}",
            failure.reason
        ),
    )
}

/// Writing a new entry stages a config edit and commits the index, so — unlike `pull`, which
/// only cares about the subrepo directory — it insists the whole tracked tree is clean.
/// Untracked files are ignored: they are never committed, and an untracked directory sitting
/// at the target path is reported by the caller's own existence check, in far clearer words.
///
/// `verb` is a gerund naming what the command is doing, e.g. "Attaching".
pub fn check_config_edit_preconditions(root: &Path, retry: &str, verb: &str) -> Option<String> {
    if rev_parse(root, "HEAD").is_none() {
        return Some(format!(
            "{} has no commits yet — commit something before {} into it. Nothing was changed.",
            root.display(),
            verb.to_lowercase(),
        ));
    }
    if !git_ok(root, &["diff", "--cached", "--quiet"]) {
        let staged = git(root, &["diff", "--cached", "--name-only"]).unwrap_or_default();
        return Some(format!(
            "you have staged changes:\n{staged}\n{verb} commits the index, so it would sweep them in. Commit or unstage them, then run `{retry}` again. Nothing was changed.",
        ));
    }
    let dirty = git(root, &["status", "--porcelain", "--untracked-files=no"]).unwrap_or_default();
    if !dirty.is_empty() {
        return Some(format!(
            "the working tree has uncommitted changes:\n{dirty}\n{verb} edits your config and commits it, so it needs a clean tree. Commit or stash them, then run `{retry}` again. Nothing was changed.",
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{resolve_config, CONFIG_FILENAME};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn entry(name: &str, path: &str, remote: &str, branch: &str) -> VendorEntry {
        VendorEntry {
            name: name.to_string(),
            path: path.to_string(),
            remote: remote.to_string(),
            branch: branch.to_string(),
            upstream: None,
            push_branch: None,
        }
    }

    fn resolved(over: VendorEntry) -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: over.name,
            path: over.path,
            remote: over.remote,
            upstream: over.upstream,
            push_branch: over.push_branch.unwrap_or_else(|| over.branch.clone()),
            branch: over.branch,
            exclude: Vec::new(),
            rewrite_message: None,
            transform: None,
            scan: None,
            monorepo_id: None,
        }
    }

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "monosplice-vendor-{tag}-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp dir");
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn project_with(dir: &TempDir, contents: &str) -> Project {
        let config_path = dir.path().join(CONFIG_FILENAME);
        fs::write(&config_path, contents).unwrap();
        let subrepos = resolve_config(contents, &config_path).unwrap();
        Project {
            root: dir.path().to_path_buf(),
            config_path,
            subrepos,
            id: None,
        }
    }

    // --- render_subrepo_entry ---

    #[test]
    fn omits_name_and_branch_when_they_equal_the_loader_defaults() {
        assert_eq!(
            render_subrepo_entry(&entry(
                "lodash",
                "vendor/lodash",
                "git@github.com:lodash/lodash.git",
                "main"
            )),
            "[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"git@github.com:lodash/lodash.git\""
        );
    }

    #[test]
    fn writes_name_and_branch_when_they_differ_from_the_defaults() {
        assert_eq!(
            render_subrepo_entry(&entry("ld", "third_party/lodash", "u", "4.17-stable")),
            "[[subrepos]]\nname = \"ld\"\npath = \"third_party/lodash\"\nremote = \"u\"\nbranch = \"4.17-stable\""
        );
    }

    #[test]
    fn escapes_quotes_and_backslashes_so_the_string_stays_valid() {
        let rendered = render_subrepo_entry(&entry("x", "vendor/x", "a\"b\\c", "main"));
        assert_eq!(
            rendered,
            "[[subrepos]]\npath = \"vendor/x\"\nremote = \"a\\\"b\\\\c\""
        );
        // ...and the escaped form is what the loader reads back.
        let subrepos = resolve_config(&rendered, Path::new("/repo/monosplice.toml")).unwrap();
        assert_eq!(subrepos[0].remote, "a\"b\\c");
    }

    #[test]
    fn renders_upstream_and_omits_a_default_push_branch() {
        let mut e = entry(
            "lodash",
            "vendor/lodash",
            "git@github.com:me/lodash.git",
            "main",
        );
        e.upstream = Some("git@github.com:lodash/lodash.git".to_string());
        e.push_branch = Some("main".to_string());
        assert_eq!(
            render_subrepo_entry(&e),
            "[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"git@github.com:me/lodash.git\"\nupstream = \"git@github.com:lodash/lodash.git\""
        );
    }

    #[test]
    fn renders_a_non_default_push_branch() {
        let mut e = entry("lodash", "vendor/lodash", "fork", "4.x");
        e.upstream = Some("up".to_string());
        e.push_branch = Some("patches".to_string());
        assert_eq!(
            render_subrepo_entry(&e),
            "[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"fork\"\nbranch = \"4.x\"\nupstream = \"up\"\npush-branch = \"patches\""
        );
    }

    #[test]
    fn a_rendered_entry_round_trips_through_the_loader() {
        let mut e = entry("ld", "third_party/lodash", "fork", "4.x");
        e.upstream = Some("up".to_string());
        e.push_branch = Some("patches".to_string());
        let subrepos = resolve_config(
            &render_subrepo_entry(&e),
            Path::new("/repo/monosplice.toml"),
        )
        .unwrap();
        assert_eq!(subrepos.len(), 1);
        let s = &subrepos[0];
        assert_eq!(s.name, "ld");
        assert_eq!(s.path, "third_party/lodash");
        assert_eq!(s.remote, "fork");
        assert_eq!(s.branch, "4.x");
        assert_eq!(s.upstream.as_deref(), Some("up"));
        assert_eq!(s.push_branch, "patches");
    }

    // --- insert_subrepo_entry ---

    #[test]
    fn appends_to_an_empty_file_without_a_leading_blank_line() {
        let block = render_subrepo_entry(&entry("lodash", "vendor/lodash", "u", "main"));
        assert_eq!(insert_subrepo_entry("", &block), format!("{block}\n"));
        assert_eq!(insert_subrepo_entry("\n\n", &block), format!("{block}\n"));
    }

    #[test]
    fn appends_after_exactly_one_blank_line_in_a_non_empty_file() {
        let block = render_subrepo_entry(&entry("lodash", "vendor/lodash", "u", "main"));
        let source = "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n";
        assert_eq!(
            insert_subrepo_entry(source, &block),
            format!("[[subrepos]]\npath = \"core\"\nremote = \"r\"\n\n{block}\n")
        );
        // Trailing blank lines do not become two blank lines, and a missing final newline is
        // not a reason to run the block onto the previous line.
        assert_eq!(
            insert_subrepo_entry("# header\n\n\n", &block),
            format!("# header\n\n{block}\n")
        );
        assert_eq!(
            insert_subrepo_entry("# header", &block),
            format!("# header\n\n{block}\n")
        );
    }

    #[test]
    fn the_appended_file_still_loads_with_both_entries_in_order() {
        let source = "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n";
        let block = render_subrepo_entry(&entry("lodash", "vendor/lodash", "u", "main"));
        let updated = insert_subrepo_entry(source, &block);
        let subrepos = resolve_config(&updated, Path::new("/repo/monosplice.toml")).unwrap();
        assert_eq!(
            subrepos.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["core", "lodash"]
        );
    }

    // --- remove_subrepo_entry (S161: the reverse of insert_subrepo_entry) ---

    #[test]
    fn removes_one_entry_among_several_and_leaves_the_rest_intact() {
        let source = "[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"u\"\n\n[[subrepos]]\npath = \"core\"\nremote = \"r\"\n";
        assert_eq!(
            remove_subrepo_entry(source, "lodash").unwrap(),
            "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n"
        );
        assert_eq!(
            remove_subrepo_entry(source, "core").unwrap(),
            "[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"u\"\n"
        );
    }

    #[test]
    fn removes_the_only_entry_leaving_an_empty_file() {
        let source = "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n";
        assert_eq!(remove_subrepo_entry(source, "core").unwrap(), "");
    }

    #[test]
    fn keeps_the_comments_and_keys_that_are_not_part_of_the_block() {
        let source = "# Monosplice configuration.\n\n[[subrepos]]\nname = \"core\"\npath = \"packages/core\"\nremote = \"r\"\nexclude = [\"a\", \"b\"]\n\n[[subrepos]]\npath = \"lib\"\nremote = \"l\"\n";
        assert_eq!(
            remove_subrepo_entry(source, "core").unwrap(),
            "# Monosplice configuration.\n\n[[subrepos]]\npath = \"lib\"\nremote = \"l\"\n"
        );
        assert_eq!(
            remove_subrepo_entry(source, "lib").unwrap(),
            "# Monosplice configuration.\n\n[[subrepos]]\nname = \"core\"\npath = \"packages/core\"\nremote = \"r\"\nexclude = [\"a\", \"b\"]\n"
        );
    }

    #[test]
    fn matches_by_the_resolved_name_so_a_quoted_key_works_too() {
        let source = "[[subrepos]]\n\"path\" = \"packages/lib\"\n\"remote\" = \"u\"\n";
        assert_eq!(remove_subrepo_entry(source, "lib").unwrap(), "");
        // A literal `name` wins over the path basename, exactly like the loader.
        let named = "[[subrepos]]\nname = \"ld\"\npath = \"packages/lib\"\nremote = \"u\"\n";
        assert!(remove_subrepo_entry(named, "lib").is_none());
        assert_eq!(remove_subrepo_entry(named, "ld").unwrap(), "");
    }

    #[test]
    fn is_not_fooled_by_brackets_inside_string_values() {
        let source = "[[subrepos]]\npath = \"core\"\nremote = \"r\"\nexclude = [\"a[b]c\"]\n\n[[subrepos]]\npath = \"lib\"\nremote = \"l\"\n";
        assert_eq!(
            remove_subrepo_entry(source, "core").unwrap(),
            "[[subrepos]]\npath = \"lib\"\nremote = \"l\"\n"
        );
    }

    #[test]
    fn returns_none_for_anything_it_cannot_locate_unambiguously() {
        let literal = "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n";
        assert!(remove_subrepo_entry(literal, "nope").is_none());
        // No block at all.
        assert!(remove_subrepo_entry("# empty\n", "core").is_none());
        // Two blocks the loader would name the same thing: the config is invalid anyway, but
        // an ambiguous match is never a licence to pick one.
        let twice = "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n\n[[subrepos]]\npath = \"a/core\"\nremote = \"r2\"\n";
        assert!(remove_subrepo_entry(twice, "core").is_none());
        // A block with no path at all, a non-string path, and a multiline string: all
        // unreadable, so even the *other* block stays put.
        for bad in [
            "[[subrepos]]\nremote = \"r\"\n",
            "[[subrepos]]\npath = 42\nremote = \"r\"\n",
            "[[subrepos]]\npath = \"\"\"lib\"\"\"\nremote = \"r\"\n",
        ] {
            let source = format!("{bad}\n[[subrepos]]\npath = \"core\"\nremote = \"r\"\n");
            assert!(
                remove_subrepo_entry(&source, "core").is_none(),
                "should refuse: {bad}"
            );
        }
    }

    // --- check_free_slot ---

    fn hints() -> SlotHints {
        SlotHints {
            rename: "Rename it".to_string(),
            relocate: "Relocate it.".to_string(),
        }
    }

    fn configured(path: &str) -> ResolvedSubrepo {
        resolved(entry("core", path, "git@github.com:you/core.git", "main"))
    }

    #[test]
    fn accepts_a_free_name_and_a_free_path() {
        let e = entry("lib", "packages/lib", "u", "main");
        assert!(check_free_slot(&[configured("core")], &e, &hints()).is_none());
        assert!(check_free_slot(&[], &e, &hints()).is_none());
    }

    #[test]
    fn rejects_a_name_that_is_taken_naming_the_subrepo_that_holds_it() {
        let e = entry("core", "packages/lib", "u", "main");
        let problem = check_free_slot(&[configured("core")], &e, &hints()).unwrap();
        assert!(problem.contains("A subrepo named core is already configured"));
        assert!(problem.contains("monosplice pull core"));
        assert!(problem.contains("Rename it"));
    }

    #[test]
    fn rejects_a_path_that_is_taken() {
        let e = entry("lib", "core", "u", "main");
        let problem = check_free_slot(&[configured("core")], &e, &hints()).unwrap();
        assert!(problem.contains("core is already configured as subrepo core"));
        assert!(problem.contains("Relocate it."));
    }

    #[test]
    fn rejects_paths_that_nest_either_way_round() {
        let inner = entry("lib", "core/inner", "u", "main");
        assert!(check_free_slot(&[configured("core")], &inner, &hints())
            .unwrap()
            .contains("may not nest"));
        let outer = entry("lib", "packages/lib", "u", "main");
        assert!(
            check_free_slot(&[configured("packages/lib/deep")], &outer, &hints())
                .unwrap()
                .contains("may not nest")
        );
    }

    #[test]
    fn does_not_treat_a_shared_prefix_as_nesting() {
        let e = entry("lib", "core", "u", "main");
        assert!(check_free_slot(&[configured("core-tools")], &e, &hints()).is_none());
    }

    // --- write_config_entry / remove_config_entry against real files ---

    #[test]
    fn write_config_entry_appends_and_verifies() {
        let dir = TempDir::new("write");
        let project = project_with(&dir, "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n");
        let new_entry = resolved(entry("lodash", "vendor/lodash", "u", "main"));

        assert!(write_config_entry(&project, &new_entry).unwrap().is_none());
        let on_disk = fs::read_to_string(&project.config_path).unwrap();
        assert_eq!(
            on_disk,
            "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n\n[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"u\"\n"
        );
    }

    #[test]
    fn write_config_entry_writes_into_a_comment_only_template() {
        let dir = TempDir::new("write-template");
        let project = project_with(
            &dir,
            "# Monosplice configuration.\n# [[subrepos]] entries go here.\n",
        );
        let new_entry = resolved(entry("lodash", "vendor/lodash", "u", "main"));

        assert!(write_config_entry(&project, &new_entry).unwrap().is_none());
        let reloaded = load_project(dir.path()).unwrap().unwrap();
        assert_eq!(reloaded.subrepos.len(), 1);
        assert_eq!(reloaded.subrepos[0].name, "lodash");
    }

    #[test]
    fn write_config_entry_reverts_when_the_reload_disagrees() {
        let dir = TempDir::new("write-revert");
        // A duplicate path makes the appended file invalid: the loader refuses it, so the
        // original bytes must come back and the caller gets the snippet.
        let original = "[[subrepos]]\nname = \"other\"\npath = \"vendor/lodash\"\nremote = \"r\"\n";
        let project = project_with(&dir, original);
        let new_entry = resolved(entry("lodash", "vendor/lodash", "u", "main"));

        let failure = write_config_entry(&project, &new_entry)
            .unwrap()
            .expect("expected a failure");
        assert!(
            failure
                .reason
                .contains("the rewritten config does not load"),
            "{}",
            failure.reason
        );
        assert!(failure.snippet.contains("[[subrepos]]"));
        assert_eq!(fs::read_to_string(&project.config_path).unwrap(), original);
    }

    #[test]
    fn remove_config_entry_cuts_the_block_and_verifies_the_rest() {
        let dir = TempDir::new("remove");
        let project = project_with(
            &dir,
            "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n\n[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"u\"\n",
        );
        let victim = project.subrepos[1].clone();

        assert!(remove_config_entry(&project, &victim).unwrap().is_none());
        assert_eq!(
            fs::read_to_string(&project.config_path).unwrap(),
            "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n"
        );
    }

    #[test]
    fn remove_config_entry_refuses_an_unreadable_neighbour_and_changes_nothing() {
        let dir = TempDir::new("remove-refuse");
        // The second block's identity can only come from a multiline-string path, which is
        // not a shape a text edit may read — so even the first block stays put.
        let original = "[[subrepos]]\npath = \"core\"\nremote = \"r\"\n\n[[subrepos]]\npath = \"\"\"vendor/lodash\"\"\"\nremote = \"u\"\n";
        let config_path = dir.path().join(CONFIG_FILENAME);
        fs::write(&config_path, original).unwrap();
        let project = Project {
            root: dir.path().to_path_buf(),
            config_path: config_path.clone(),
            subrepos: resolve_config(original, &config_path).unwrap(),
            id: None,
        };
        let victim = project.subrepos[0].clone();

        let failure = remove_config_entry(&project, &victim)
            .unwrap()
            .expect("expected a failure");
        assert!(
            failure
                .reason
                .contains("no plain [[subrepos]] entry for core"),
            "{}",
            failure.reason
        );
        assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
    }

    // --- the paste/delete instructions ---

    #[test]
    fn paste_it_yourself_prints_the_snippet_then_names_the_retry() {
        let failure = ConfigWriteFailure {
            snippet: "[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"u\"".to_string(),
            reason: "the rewritten config does not load".to_string(),
        };
        let (log, error) = paste_it_yourself(
            Path::new("/repo/monosplice.toml"),
            &failure,
            "monosplice attach vendor/lodash u",
        );
        assert_eq!(
            log,
            "Add this [[subrepos]] entry to /repo/monosplice.toml:\n\n[[subrepos]]\npath = \"vendor/lodash\"\nremote = \"u\"\n"
        );
        assert!(error.starts_with("monosplice cannot safely edit /repo/monosplice.toml: the rewritten config does not load.\n"));
        assert!(error.ends_with("\n  monosplice attach vendor/lodash u"));
    }

    #[test]
    fn delete_it_yourself_names_the_entry_and_promises_the_directory_is_safe() {
        let subrepo = resolved(entry("lodash", "vendor/lodash", "u", "main"));
        let failure = ConfigRemoveFailure {
            reason: "the rewritten config still has a subrepo named lodash".to_string(),
        };
        let (log, error) =
            delete_it_yourself(Path::new("/repo/monosplice.toml"), &subrepo, &failure);
        assert_eq!(
            log,
            "Delete the [[subrepos]] entry for lodash (vendor/lodash/ tracking u) from /repo/monosplice.toml.\n"
        );
        assert!(
            error.contains("Nothing was changed — the config is untouched and no commit was made.")
        );
        assert!(
            error.contains("vendor/lodash/ and its history stay exactly as they are either way.")
        );
    }
}
