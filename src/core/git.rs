//! Talking to git. Every call shells out to the system `git` (see CLAUDE.md); nothing
//! here ever touches the working tree except by the caller's explicit request.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::core::trailers::{SyncTrailer, ORIGIN_TRAILER, SOURCE_TRAILER};

/// SHA of git's canonical empty tree object.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug)]
pub struct GitError {
    pub git_args: Vec<String>,
    pub exit_code: Option<i32>,
    pub stderr: String,
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = match self.exit_code {
            Some(c) => c.to_string(),
            None => "unknown".to_string(),
        };
        write!(
            f,
            "git {} failed (exit {})\n{}",
            self.git_args.join(" "),
            code,
            self.stderr
        )
    }
}

impl std::error::Error for GitError {}

#[derive(Debug, Clone)]
pub struct CommitMeta {
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    /// raw format: "<unix-ts> <tz>"
    pub author_date: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_date: String,
    /// Full raw message including trailers.
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub mode: String,
    /// "blob" | "commit" | "tree"
    pub kind: String,
    pub sha: String,
    /// Path relative to the listed tree root.
    pub path: String,
}

/// Options for a git invocation. `env` is layered on top of the inherited environment
/// (execa's `extendEnv` default); `input` is written to git's stdin.
#[derive(Default)]
pub struct GitOpts<'a> {
    pub env: &'a [(&'a str, String)],
    pub input: Option<&'a [u8]>,
}

/// execa's `stripFinalNewline`: exactly one trailing "\n" (with a preceding "\r" if
/// present) comes off. Not `trim()` — commit messages and blob-ish output keep their
/// own leading and inner whitespace.
fn strip_final_newline(mut out: Vec<u8>) -> Vec<u8> {
    if out.last() == Some(&b'\n') {
        out.pop();
        if out.last() == Some(&b'\r') {
            out.pop();
        }
    }
    out
}

/// The `--stdin` batch payload: one name per line, every line terminated.
fn stdin_list(shas: &[String]) -> String {
    let mut s = String::new();
    for sha in shas {
        s.push_str(sha);
        s.push('\n');
    }
    s
}

/// Split git output into lines the way the TS did: empty output means no lines at all
/// (a bare `split('\n')` would yield one empty string).
fn split_lines(out: &str) -> Vec<String> {
    if out.is_empty() {
        Vec::new()
    } else {
        out.split('\n').map(str::to_string).collect()
    }
}

fn parse_commit_meta(out: &str) -> Option<CommitMeta> {
    let parts: Vec<&str> = out.split('\0').collect();
    if parts.len() < 8 {
        return None;
    }
    Some(CommitMeta {
        sha: parts[0].to_string(),
        author_name: parts[1].to_string(),
        author_email: parts[2].to_string(),
        author_date: parts[3].to_string(),
        committer_name: parts[4].to_string(),
        committer_email: parts[5].to_string(),
        committer_date: parts[6].to_string(),
        // A message may itself contain NULs; the tail is rejoined, never truncated.
        message: parts[7..].join("\0"),
    })
}

fn parse_commit_subjects(out: &str) -> HashMap<String, String> {
    let mut subjects = HashMap::new();
    if out.is_empty() {
        return subjects;
    }
    for line in out.split('\n') {
        let mut fields = line.split('\0');
        let sha = fields.next().unwrap_or("");
        let subject = fields.next().unwrap_or("");
        if !sha.is_empty() {
            subjects.insert(sha.to_string(), subject.to_string());
        }
    }
    subjects
}

fn parse_missing(out: &str) -> HashSet<String> {
    let mut missing = HashSet::new();
    if out.is_empty() {
        return missing;
    }
    for line in out.split('\n') {
        // git answers "<input> missing" for anything it cannot resolve.
        let mut fields = line.split(' ');
        let name = fields.next().unwrap_or("");
        let status = fields.next().unwrap_or("");
        if !name.is_empty() && status == "missing" {
            missing.insert(name.to_string());
        }
    }
    missing
}

fn parse_existing_commits(out: &str) -> Vec<String> {
    let mut ok = Vec::new();
    if out.is_empty() {
        return ok;
    }
    for line in out.split('\n') {
        let mut fields = line.split(' ');
        let name = fields.next().unwrap_or("");
        let kind = fields.next().unwrap_or("");
        if !name.is_empty() && kind == "commit" {
            ok.push(name.to_string());
        }
    }
    ok
}

fn parse_ls_tree(out: &str) -> Vec<TreeEntry> {
    if out.is_empty() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    for line in out.split('\0').filter(|l| !l.is_empty()) {
        let Some(tab) = line.find('\t') else { continue };
        let mut head = line[..tab].split(' ');
        let (Some(mode), Some(kind), Some(sha)) = (head.next(), head.next(), head.next()) else {
            continue;
        };
        entries.push(TreeEntry {
            mode: mode.to_string(),
            kind: kind.to_string(),
            sha: sha.to_string(),
            // Only the first tab separates metadata from the path; paths may contain more.
            path: line[tab + 1..].to_string(),
        });
    }
    entries
}

/// Split flat entries into the ones living at this level and, per first path segment,
/// the ones below it (with that segment stripped). Subdirectory order is first-seen,
/// matching the JS `Map` the TS built.
#[allow(clippy::type_complexity)]
fn group_entries(entries: &[TreeEntry]) -> (Vec<TreeEntry>, Vec<(String, Vec<TreeEntry>)>) {
    let mut here: Vec<TreeEntry> = Vec::new();
    let mut subdirs: Vec<(String, Vec<TreeEntry>)> = Vec::new();
    for e in entries {
        match e.path.find('/') {
            None => here.push(e.clone()),
            Some(slash) => {
                let dir = e.path[..slash].to_string();
                let rest = e.path[slash + 1..].to_string();
                let child = TreeEntry {
                    path: rest,
                    ..e.clone()
                };
                match subdirs.iter_mut().find(|(d, _)| *d == dir) {
                    Some((_, children)) => children.push(child),
                    None => subdirs.push((dir, vec![child])),
                }
            }
        }
    }
    (here, subdirs)
}

fn mktree_input(lines: &[String]) -> String {
    let mut s = String::new();
    for l in lines {
        s.push_str(l);
        s.push('\0');
    }
    s
}

fn parse_sync_trailers(out: &str) -> HashMap<String, Vec<SyncTrailer>> {
    let mut map: HashMap<String, Vec<SyncTrailer>> = HashMap::new();
    if out.is_empty() {
        return map;
    }
    for line in out.split('\n') {
        let mut fields = line.split('\0');
        let sha = fields.next().unwrap_or("");
        let trailers: Vec<SyncTrailer> = fields.filter_map(SyncTrailer::parse).collect();
        if !sha.is_empty() && !trailers.is_empty() {
            map.insert(sha.to_string(), trailers);
        }
    }
    map
}

fn parse_ls_remote(out: &str) -> Option<String> {
    if out.is_empty() {
        return None;
    }
    Some(out.split('\t').next().unwrap_or("").to_string())
}

fn owned_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

/// Run git and hand back raw stdout bytes plus the exit status. The only place a
/// process is actually started (`probe_push_access` aside, which needs its own timeout).
fn run_git(cwd: &Path, args: &[&str], opts: &GitOpts) -> Result<Vec<u8>, GitError> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in opts.env {
        cmd.env(k, v);
    }
    if opts.input.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::inherit());
    }
    let mut child = cmd.spawn().map_err(|e| GitError {
        git_args: owned_args(args),
        exit_code: None,
        stderr: e.to_string(),
    })?;

    let writer = match opts.input {
        Some(input) => {
            let data = input.to_vec();
            child.stdin.take().map(|mut stdin| {
                // Feeding stdin from another thread is what keeps a large `--stdin` batch
                // from deadlocking against git's own output. A git that exits early gives
                // us EPIPE, which is not an error here (execa ignores it too).
                thread::spawn(move || {
                    let _ = stdin.write_all(&data);
                })
            })
        }
        None => None,
    };

    let output = child.wait_with_output().map_err(|e| GitError {
        git_args: owned_args(args),
        exit_code: None,
        stderr: e.to_string(),
    })?;
    if let Some(handle) = writer {
        let _ = handle.join();
    }

    if !output.status.success() {
        return Err(GitError {
            git_args: owned_args(args),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout)
}

/// Run git in `cwd`, return stdout with its final newline stripped. Errors on non-zero exit.
pub fn git(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    git_with(cwd, args, GitOpts::default())
}

pub fn git_with(cwd: &Path, args: &[&str], opts: GitOpts) -> Result<String, GitError> {
    let out = run_git(cwd, args, &opts)?;
    Ok(String::from_utf8_lossy(&strip_final_newline(out)).into_owned())
}

/// Run git and return raw stdout bytes (for blob contents) — never stripped.
pub fn git_buffer(cwd: &Path, args: &[&str], opts: GitOpts) -> Result<Vec<u8>, GitError> {
    run_git(cwd, args, &opts)
}

/// Run git, return true on exit 0, false on any failure.
pub fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn read_commit(cwd: &Path, sha: &str) -> Result<CommitMeta, GitError> {
    let args = [
        "show",
        "-s",
        "--date=raw",
        "--format=%H%x00%an%x00%ae%x00%ad%x00%cn%x00%ce%x00%cd%x00%B",
        sha,
    ];
    let out = git(cwd, &args)?;
    parse_commit_meta(&out).ok_or_else(|| GitError {
        git_args: owned_args(&args),
        exit_code: None,
        stderr: format!("unexpected git show output for {sha}"),
    })
}

/// rev-list; returns [] for empty output. Extra args like --reverse, ranges, `--`, paths.
pub fn rev_list(cwd: &Path, args: &[&str]) -> Result<Vec<String>, GitError> {
    let mut full: Vec<&str> = vec!["rev-list"];
    full.extend_from_slice(args);
    let out = git(cwd, &full)?;
    Ok(split_lines(&out))
}

/// Subject line per commit, in one process regardless of how many were asked for
/// (`--stdin`, because a pending list can be thousands long). Order is the caller's: the
/// map is keyed by sha precisely so it does not depend on git's own sort.
pub fn commit_subjects(cwd: &Path, shas: &[String]) -> Result<HashMap<String, String>, GitError> {
    if shas.is_empty() {
        return Ok(HashMap::new());
    }
    let input = stdin_list(shas);
    let out = git_with(
        cwd,
        &["log", "--no-walk", "--format=%H%x00%s", "--stdin"],
        GitOpts {
            input: Some(input.as_bytes()),
            ..Default::default()
        },
    )?;
    Ok(parse_commit_subjects(&out))
}

pub fn rev_parse(cwd: &Path, r: &str) -> Option<String> {
    let spec = format!("{r}^{{commit}}");
    git(cwd, &["rev-parse", "--verify", "--quiet", &spec]).ok()
}

/// Which of these object ids are absent from the local object db. One batched
/// `cat-file` process regardless of input size — trailer scans can name thousands.
pub fn missing_objects(cwd: &Path, shas: &[String]) -> Result<HashSet<String>, GitError> {
    if shas.is_empty() {
        return Ok(HashSet::new());
    }
    let out = batch_check(cwd, shas)?;
    Ok(parse_missing(&out))
}

/// Which of these object ids name commits that really exist here. Same single batched
/// `cat-file` as `missing_objects`; used to sanitize trailer values before feeding them to
/// `rev-list`, where one unknown name would abort the whole query.
pub fn existing_commits(cwd: &Path, shas: &[String]) -> Result<Vec<String>, GitError> {
    if shas.is_empty() {
        return Ok(Vec::new());
    }
    let out = batch_check(cwd, shas)?;
    Ok(parse_existing_commits(&out))
}

fn batch_check(cwd: &Path, shas: &[String]) -> Result<String, GitError> {
    let input = stdin_list(shas);
    git_with(
        cwd,
        &["cat-file", "--batch-check"],
        GitOpts {
            input: Some(input.as_bytes()),
            ..Default::default()
        },
    )
}

pub struct CommitTreeInput {
    pub tree: String,
    pub parents: Vec<String>,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub author_date: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_date: String,
}

/// Create a commit object directly in the object db. Never touches the working tree.
pub fn commit_tree(cwd: &Path, input: &CommitTreeInput) -> Result<String, GitError> {
    let mut args: Vec<&str> = vec!["commit-tree", &input.tree];
    for p in &input.parents {
        args.push("-p");
        args.push(p);
    }
    let env = [
        ("GIT_AUTHOR_NAME", input.author_name.clone()),
        ("GIT_AUTHOR_EMAIL", input.author_email.clone()),
        ("GIT_AUTHOR_DATE", input.author_date.clone()),
        ("GIT_COMMITTER_NAME", input.committer_name.clone()),
        ("GIT_COMMITTER_EMAIL", input.committer_email.clone()),
        ("GIT_COMMITTER_DATE", input.committer_date.clone()),
    ];
    git_with(
        cwd,
        &args,
        GitOpts {
            env: &env,
            input: Some(input.message.as_bytes()),
        },
    )
}

/// Recursive listing of a tree-ish: blobs, symlinks (mode 120000) and submodule entries.
pub fn ls_tree_recursive(cwd: &Path, treeish: &str) -> Result<Vec<TreeEntry>, GitError> {
    let out = git(cwd, &["ls-tree", "-r", "-z", treeish])?;
    Ok(parse_ls_tree(&out))
}

/// Build a (possibly nested) tree object from flat entries and return its sha.
/// Entries' paths are relative to the tree being built.
pub fn build_tree(cwd: &Path, entries: &[TreeEntry]) -> Result<String, GitError> {
    let (here, subdirs) = group_entries(entries);
    let mut lines: Vec<String> = here
        .iter()
        .map(|e| format!("{} {} {}\t{}", e.mode, e.kind, e.sha, e.path))
        .collect();
    for (dir, children) in subdirs {
        let sub = build_tree(cwd, &children)?;
        lines.push(format!("040000 tree {sub}\t{dir}"));
    }
    let input = mktree_input(&lines);
    git_with(
        cwd,
        &["mktree", "-z"],
        GitOpts {
            input: Some(input.as_bytes()),
            ..Default::default()
        },
    )
}

/// Write blob content into the object db, return sha.
pub fn hash_object(cwd: &Path, data: &[u8]) -> Result<String, GitError> {
    git_with(
        cwd,
        &["hash-object", "-w", "--stdin"],
        GitOpts {
            input: Some(data),
            ..Default::default()
        },
    )
}

/// Read a blob's raw content.
pub fn read_blob(cwd: &Path, sha: &str) -> Result<Vec<u8>, GitError> {
    git_buffer(cwd, &["cat-file", "blob", sha], GitOpts::default())
}

/// Every commit in the given rev range that carries at least one monosplice sync trailer,
/// mapped to those trailers *in message order*. The order matters: the last one is the claim of
/// the hop that wrote the commit ([`crate::core::trailers::writer_trailer`] has the rule), and
/// any before it were forwarded from an earlier hop. Parsed by git itself, as it always was, so
/// what counts as a trailer block does not change. `rev_args` example: ["HEAD"] or ["A..B"].
pub fn sync_trailers(
    cwd: &Path,
    rev_args: &[&str],
) -> Result<HashMap<String, Vec<SyncTrailer>>, GitError> {
    let format = format!(
        "--format=%H%x00%(trailers:key={SOURCE_TRAILER},key={ORIGIN_TRAILER},separator=%x00)"
    );
    let mut args: Vec<&str> = vec!["log", &format];
    args.extend_from_slice(rev_args);
    let out = git(cwd, &args)?;
    Ok(parse_sync_trailers(&out))
}

/// Is this clone shallow? Commits beyond the shallow boundary are missing without having been
/// rewritten, so a trailer naming one could be anybody's.
pub fn is_shallow(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-shallow-repository"]).is_ok_and(|out| out == "true")
}

/// Resolve a branch head on a remote. Returns the sha, None if the branch (or an
/// empty repo) has no such ref, and a GitError if the remote is unreachable.
pub fn ls_remote_branch(
    cwd: &Path,
    remote: &str,
    branch: &str,
) -> Result<Option<String>, GitError> {
    let refname = format!("refs/heads/{branch}");
    let out = git(cwd, &["ls-remote", remote, &refname])?;
    Ok(parse_ls_remote(&out))
}

/// Fetch a remote branch into a local tracking ref; returns the fetched head sha.
pub fn fetch_branch(
    cwd: &Path,
    remote: &str,
    branch: &str,
    local_ref: &str,
) -> Result<String, GitError> {
    let refspec = format!("+refs/heads/{branch}:{local_ref}");
    git(cwd, &["fetch", "--no-tags", remote, &refspec])?;
    git(cwd, &["rev-parse", local_ref])
}

/// Push a local sha to a remote ref (fast-forward only — no force).
pub fn push_ref(cwd: &Path, remote: &str, sha: &str, dst_ref: &str) -> Result<(), GitError> {
    let refspec = format!("{sha}:{dst_ref}");
    git(cwd, &["push", remote, &refspec])?;
    Ok(())
}

/// Replace a remote ref, but only while it still holds `expect`. Used for the one ref
/// monosplice owns outright — the fork's push branch, which is rebuilt from upstream on every
/// push — so that a rewrite still refuses when somebody else moved the branch in the meantime.
pub fn push_ref_with_lease(
    cwd: &Path,
    remote: &str,
    sha: &str,
    dst_ref: &str,
    expect: &str,
) -> Result<(), GitError> {
    let lease = format!("--force-with-lease={dst_ref}:{expect}");
    let refspec = format!("{sha}:{dst_ref}");
    git(cwd, &["push", &lease, remote, &refspec])?;
    Ok(())
}

/// How long `probe_push_access` waits before calling a remote unreachable.
const PROBE_TIMEOUT_MS: u64 = 20_000;

/// Advisory write-access check: a dry-run push of a ref to exactly where it already points.
/// Nothing is ever written — but the transport still has to open `receive-pack` on the far
/// side, which is where a remote you can only read from says no. Returns None when the push
/// would be allowed, or git's own complaint when it would not.
///
/// Never prompts (`GIT_TERMINAL_PROMPT=0`) and never hangs the command it decorates: a remote
/// that stops answering is reported as "could not tell", not waited on.
pub fn probe_push_access(cwd: &Path, remote: &str, sha: &str, branch: &str) -> Option<String> {
    let refspec = format!("{sha}:refs/heads/{branch}");
    let args = ["push", "--dry-run", remote, &refspec];
    let rendered = || format!("git {}", args.join(" "));

    let mut child = match Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Some(e.to_string()),
    };

    let Some(mut pipe) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Some(format!("{}: could not capture stderr", rendered()));
    };

    // The waiter thread drains stderr; EOF on that pipe is git letting go of it. Everything
    // stays bounded by the timeout, including a transport that answers with silence.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    match rx.recv_timeout(Duration::from_millis(PROBE_TIMEOUT_MS)) {
        Ok(buf) => {
            let status = match child.wait() {
                Ok(s) => s,
                Err(e) => return Some(e.to_string()),
            };
            if status.success() {
                return None;
            }
            let stderr = String::from_utf8_lossy(&buf).trim().to_string();
            if stderr.is_empty() {
                let code = match status.code() {
                    Some(c) => c.to_string(),
                    None => "unknown".to_string(),
                };
                Some(format!("{} failed (exit {})", rendered(), code))
            } else {
                Some(stderr)
            }
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Some(format!(
                "Command timed out after {PROBE_TIMEOUT_MS} milliseconds: {}",
                rendered()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_exactly_one_trailing_newline() {
        assert_eq!(strip_final_newline(b"abc\n".to_vec()), b"abc".to_vec());
        assert_eq!(strip_final_newline(b"abc\r\n".to_vec()), b"abc".to_vec());
        assert_eq!(strip_final_newline(b"abc\n\n".to_vec()), b"abc\n".to_vec());
        assert_eq!(strip_final_newline(b"abc".to_vec()), b"abc".to_vec());
        assert_eq!(strip_final_newline(b"".to_vec()), b"".to_vec());
        assert_eq!(strip_final_newline(b"\n".to_vec()), b"".to_vec());
        // Leading/inner whitespace survives — this is not trim().
        assert_eq!(
            strip_final_newline(b"  a b \n".to_vec()),
            b"  a b ".to_vec()
        );
    }

    #[test]
    fn git_error_renders_like_the_ts_message() {
        let err = GitError {
            git_args: vec!["push".to_string(), "origin".to_string()],
            exit_code: Some(128),
            stderr: "boom".to_string(),
        };
        assert_eq!(err.to_string(), "git push origin failed (exit 128)\nboom");
        let killed = GitError {
            git_args: vec!["log".to_string()],
            exit_code: None,
            stderr: String::new(),
        };
        assert_eq!(killed.to_string(), "git log failed (exit unknown)\n");
    }

    #[test]
    fn builds_newline_terminated_stdin_batches() {
        assert_eq!(stdin_list(&[]), "");
        assert_eq!(
            stdin_list(&["a".to_string(), "b".to_string()]),
            "a\nb\n".to_string()
        );
    }

    #[test]
    fn empty_output_is_no_lines() {
        assert!(split_lines("").is_empty());
        assert_eq!(split_lines("a"), vec!["a".to_string()]);
        assert_eq!(split_lines("a\nb"), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parses_commit_meta_nul_fields() {
        let out =
            "SHA\0An\0ae@x\x001700000000 +0000\0Cn\0ce@x\x001700000060 +0000\0subject\n\nbody\n";
        let m = parse_commit_meta(out).expect("parses");
        assert_eq!(m.sha, "SHA");
        assert_eq!(m.author_name, "An");
        assert_eq!(m.author_email, "ae@x");
        assert_eq!(m.author_date, "1700000000 +0000");
        assert_eq!(m.committer_name, "Cn");
        assert_eq!(m.committer_email, "ce@x");
        assert_eq!(m.committer_date, "1700000060 +0000");
        assert_eq!(m.message, "subject\n\nbody\n");
    }

    #[test]
    fn a_message_containing_nul_is_rejoined() {
        let out = "SHA\0An\0ae\0d1\0Cn\0ce\0d2\0msg\0with-nul";
        let m = parse_commit_meta(out).expect("parses");
        assert_eq!(m.message, "msg\0with-nul");
    }

    #[test]
    fn short_commit_meta_output_is_an_error() {
        assert!(parse_commit_meta("SHA\0An\0ae").is_none());
    }

    #[test]
    fn parses_subjects_keyed_by_sha() {
        let out = "aaa\0first subject\nbbb\0second: subject\nccc\0";
        let subjects = parse_commit_subjects(out);
        assert_eq!(
            subjects.get("aaa").map(String::as_str),
            Some("first subject")
        );
        assert_eq!(
            subjects.get("bbb").map(String::as_str),
            Some("second: subject")
        );
        assert_eq!(subjects.get("ccc").map(String::as_str), Some(""));
        assert_eq!(subjects.len(), 3);
        assert!(parse_commit_subjects("").is_empty());
    }

    #[test]
    fn parses_batch_check_missing_objects() {
        let out = "aaa missing\nbbb commit 217\nccc missing";
        let missing = parse_missing(out);
        assert!(missing.contains("aaa"));
        assert!(missing.contains("ccc"));
        assert!(!missing.contains("bbb"));
        assert_eq!(missing.len(), 2);
        assert!(parse_missing("").is_empty());
    }

    #[test]
    fn parses_batch_check_existing_commits() {
        let out = "aaa commit 217\nbbb missing\nccc tree 40\nddd commit 12";
        assert_eq!(
            parse_existing_commits(out),
            vec!["aaa".to_string(), "ddd".to_string()]
        );
        assert!(parse_existing_commits("").is_empty());
    }

    #[test]
    fn parses_nul_separated_ls_tree() {
        let out = "100644 blob aaa\tREADME.md\x00120000 blob bbb\tlink\x00160000 commit ccc\tvendor/dep\x00100755 blob ddd\tbin/run.sh\0";
        let entries = parse_ls_tree(out);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].mode, "100644");
        assert_eq!(entries[0].kind, "blob");
        assert_eq!(entries[0].sha, "aaa");
        assert_eq!(entries[0].path, "README.md");
        assert_eq!(entries[2].kind, "commit");
        assert_eq!(entries[2].path, "vendor/dep");
        assert_eq!(entries[3].mode, "100755");
        assert_eq!(entries[3].path, "bin/run.sh");
        assert!(parse_ls_tree("").is_empty());
    }

    #[test]
    fn ls_tree_keeps_paths_with_spaces_and_tabs_intact() {
        let out = "100644 blob aaa\tdocs/a b\tc.md\0";
        let entries = parse_ls_tree(out);
        assert_eq!(entries[0].path, "docs/a b\tc.md");
    }

    #[test]
    fn groups_entries_by_first_path_segment_in_order() {
        let entries = vec![
            entry("100644", "blob", "a1", "README.md"),
            entry("100644", "blob", "b1", "src/lib.rs"),
            entry("100644", "blob", "c1", "docs/x.md"),
            entry("100644", "blob", "d1", "src/core/git.rs"),
        ];
        let (here, subdirs) = group_entries(&entries);
        assert_eq!(here.len(), 1);
        assert_eq!(here[0].path, "README.md");
        // Insertion order of the JS Map: src first, then docs.
        assert_eq!(subdirs.len(), 2);
        assert_eq!(subdirs[0].0, "src");
        assert_eq!(
            subdirs[0]
                .1
                .iter()
                .map(|e| e.path.clone())
                .collect::<Vec<_>>(),
            vec!["lib.rs".to_string(), "core/git.rs".to_string()]
        );
        assert_eq!(subdirs[1].0, "docs");
        assert_eq!(subdirs[1].1[0].path, "x.md");
        assert_eq!(subdirs[1].1[0].sha, "c1");
    }

    #[test]
    fn mktree_input_is_nul_terminated_lines() {
        let lines = vec![
            "100644 blob aaa\tREADME.md".to_string(),
            "040000 tree bbb\tsrc".to_string(),
        ];
        assert_eq!(
            mktree_input(&lines),
            "100644 blob aaa\tREADME.md\x00040000 tree bbb\tsrc\0"
        );
        assert_eq!(mktree_input(&[]), "");
    }

    #[test]
    fn parses_sync_trailers_in_order_dropping_commits_without_any() {
        let out = "aaa\0Monosplice-Source: mono1\nbbb\0\nccc\0Monosplice-Origin: l1\0monosplice-origin:  m1 \nddd\0\0";
        let map = parse_sync_trailers(out);
        assert_eq!(
            map.get("aaa"),
            Some(&vec![SyncTrailer::Source("mono1".to_string())])
        );
        assert_eq!(map.get("bbb"), None);
        assert_eq!(
            map.get("ccc"),
            Some(&vec![
                SyncTrailer::Origin("l1".to_string()),
                SyncTrailer::Origin("m1".to_string())
            ])
        );
        assert_eq!(map.get("ddd"), None);
        assert_eq!(map.len(), 2);
        assert!(parse_sync_trailers("").is_empty());
    }

    #[test]
    fn parses_ls_remote_output() {
        assert_eq!(parse_ls_remote(""), None);
        assert_eq!(
            parse_ls_remote("abc123\trefs/heads/main").as_deref(),
            Some("abc123")
        );
    }

    fn entry(mode: &str, kind: &str, sha: &str, path: &str) -> TreeEntry {
        TreeEntry {
            mode: mode.to_string(),
            kind: kind.to_string(),
            sha: sha.to_string(),
            path: path.to_string(),
        }
    }
}

#[cfg(test)]
mod smoke {
    use super::*;
    use std::path::PathBuf;

    fn sh(cwd: &Path, cmd: &str) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Exercises the process-running half of this module against a real repo: the object-db
    /// plumbing (`build_tree` must reproduce git's own tree sha), the `--stdin` batching, the
    /// env/stdin wiring, and the remote ops. Kept hermetic — no global/system git config.
    #[test]
    fn smoke_against_a_real_repo() {
        // Isolated from the developer's own git config, exactly like the e2e harness.
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");

        let dir: PathBuf = std::env::temp_dir().join(format!("ms-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.as_path();
        sh(d, "git init -q -b main .");
        sh(
            d,
            "git config user.email a@b.c && git config user.name 'A B'",
        );
        sh(
            d,
            "mkdir -p sub && printf 'hello\n' > README.md && printf 'x\n' > sub/x.txt",
        );
        sh(
            d,
            "git add -A && git commit -q -m 'first

Monosplice-Source: deadbeef
'",
        );
        sh(
            d,
            "printf 'again\n' >> README.md && git add -A && git commit -q -m 'second commit'",
        );

        let head = rev_parse(d, "HEAD").expect("head");
        assert_eq!(head.len(), 40);
        assert!(rev_parse(d, "nope").is_none());
        assert!(git_ok(d, &["rev-parse", "--git-dir"]));
        assert!(!git_ok(d, &["rev-parse", "--verify", "nope"]));

        let commits = rev_list(d, &["HEAD"]).unwrap();
        assert_eq!(commits.len(), 2);
        let subjects = commit_subjects(d, &commits).unwrap();
        assert_eq!(subjects.get(&commits[0]).unwrap(), "second commit");
        assert_eq!(subjects.get(&commits[1]).unwrap(), "first");

        let meta = read_commit(d, &commits[1]).unwrap();
        assert_eq!(meta.author_name, "A B");
        assert_eq!(meta.author_email, "a@b.c");
        assert!(meta
            .message
            .starts_with("first\n\nMonosplice-Source: deadbeef"));
        assert!(meta.author_date.contains(' '));

        let tv = sync_trailers(d, &["HEAD"]).unwrap();
        assert_eq!(tv.len(), 1);
        assert_eq!(
            tv.get(&commits[1]).unwrap(),
            &vec![SyncTrailer::Source("deadbeef".to_string())]
        );
        assert!(!is_shallow(d));

        let entries = ls_tree_recursive(d, "HEAD").unwrap();
        assert_eq!(entries.len(), 2);
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"README.md") && paths.contains(&"sub/x.txt"));

        let tree = build_tree(d, &entries).unwrap();
        assert_eq!(tree, sh(d, "git rev-parse HEAD^{tree}"));

        let blob = hash_object(d, b"payload\n").unwrap();
        assert_eq!(read_blob(d, &blob).unwrap(), b"payload\n".to_vec());
        // git_buffer never strips.
        assert_eq!(
            git_buffer(d, &["cat-file", "blob", &blob], GitOpts::default()).unwrap(),
            b"payload\n".to_vec()
        );

        let missing = missing_objects(d, &[blob.clone(), "0".repeat(40)]).unwrap();
        assert_eq!(missing.len(), 1);
        assert!(missing.contains(&"0".repeat(40)));
        let existing = existing_commits(d, &[blob.clone(), head.clone(), "0".repeat(40)]).unwrap();
        assert_eq!(existing, vec![head.clone()]);

        let new_sha = commit_tree(
            d,
            &CommitTreeInput {
                tree,
                parents: vec![head.clone()],
                message: "made by commit_tree\n".to_string(),
                author_name: "Au Thor".to_string(),
                author_email: "au@x.y".to_string(),
                author_date: "1700000000 +0000".to_string(),
                committer_name: "Com Mitter".to_string(),
                committer_email: "com@x.y".to_string(),
                committer_date: "1700000060 +0000".to_string(),
            },
        )
        .unwrap();
        let m2 = read_commit(d, &new_sha).unwrap();
        assert_eq!(m2.author_name, "Au Thor");
        assert_eq!(m2.author_date, "1700000000 +0000");
        assert_eq!(m2.committer_email, "com@x.y");
        assert_eq!(m2.message, "made by commit_tree\n");

        // env merges on top of the inherited environment (PATH still resolves git).
        let env = [("GIT_AUTHOR_NAME", "Env Person".to_string())];
        let out = git_with(
            d,
            &["var", "GIT_AUTHOR_IDENT"],
            GitOpts {
                env: &env,
                input: None,
            },
        )
        .unwrap();
        assert!(out.starts_with("Env Person <a@b.c>"), "{out}");

        // errors carry args, code and stderr
        let err = git(d, &["rev-parse", "--verify", "nope"]).unwrap_err();
        assert_eq!(err.git_args, vec!["rev-parse", "--verify", "nope"]);
        assert_eq!(err.exit_code, Some(128));
        assert!(err
            .to_string()
            .starts_with("git rev-parse --verify nope failed (exit 128)\n"));

        // remotes
        let bare = dir.join("remote.git");
        sh(d, &format!("git init -q --bare {}", bare.display()));
        assert_eq!(
            ls_remote_branch(d, bare.to_str().unwrap(), "main").unwrap(),
            None
        );
        push_ref(d, bare.to_str().unwrap(), &head, "refs/heads/main").unwrap();
        assert_eq!(
            ls_remote_branch(d, bare.to_str().unwrap(), "main").unwrap(),
            Some(head.clone())
        );
        let fetched =
            fetch_branch(d, bare.to_str().unwrap(), "main", "refs/monosplice/tmp").unwrap();
        assert_eq!(fetched, head);
        push_ref_with_lease(
            d,
            bare.to_str().unwrap(),
            &new_sha,
            "refs/heads/main",
            &head,
        )
        .unwrap();
        assert_eq!(
            ls_remote_branch(d, bare.to_str().unwrap(), "main").unwrap(),
            Some(new_sha.clone())
        );
        assert!(
            push_ref_with_lease(d, bare.to_str().unwrap(), &head, "refs/heads/main", &head)
                .is_err()
        );

        // probe: writable local remote is allowed; a bogus remote is refused, not hung.
        assert_eq!(
            probe_push_access(d, bare.to_str().unwrap(), &new_sha, "main"),
            None
        );
        let bad = probe_push_access(d, "/nonexistent/repo.git", &head, "main");
        assert!(bad.is_some(), "expected a complaint");

        // long --stdin batch does not deadlock
        let many: Vec<String> = (0..5000).map(|_| head.clone()).collect();
        assert_eq!(commit_subjects(d, &many).unwrap().len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
