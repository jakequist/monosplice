//! Replaying standalone-repo commits into the monorepo.
//!
//! Import is the only monosplice operation allowed to write the working tree and index
//! (see CLAUDE.md): a replay is a merge the user may have to resolve, so it happens where
//! the user can see it. Everything interrupted lives in a sequencer under the git dir.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::ResolvedSubrepo;
use crate::core::git::{
    git, git_buffer, git_ok, git_with, read_commit, rev_list, rev_parse, GitError, GitOpts,
    EMPTY_TREE,
};
use crate::core::paths::make_excluder;
use crate::core::trailers::{append_trailer, strip_sync_trailers, ORIGIN_TRAILER};

/// The standalone-repo commit currently being replayed, captured so `--continue` can finish it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullSequencerCommit {
    pub sha: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    /// raw format: "<unix-ts> <tz>"
    pub author_date: String,
}

/// Transient state for an interrupted import. Lives under the git dir, never in the work
/// tree and never committed: it is a sequencer like `.git/rebase-merge`, not project state.
///
/// The last three fields exist so `--abort` can put the monorepo back exactly as it was: the
/// subrepo directory bounds what abort is allowed to touch, `startHead` is where the run
/// began, and `created` is the proof that everything between the two is monosplice's own work.
///
/// The optional fields stay optional so a sequencer written by the TypeScript CLI — which
/// predates them — still loads; every sequencer this code writes carries all three.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullSequencer {
    pub subrepo: String,
    /// Subrepo directory, so `--abort` still works if the config entry was removed meanwhile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub current: PullSequencerCommit,
    pub remaining: Vec<String>,
    /// Monorepo HEAD before this pull run committed anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_head: Option<String>,
    /// Monorepo commits this run created before the conflict, oldest first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<Vec<String>>,
}

/// Where a pull run started and what it has committed so far, carried across `--continue`.
#[derive(Debug, Clone)]
pub struct RunProvenance {
    pub start_head: String,
    pub created: Vec<String>,
}

/// A replay that stopped on a three-way merge the user has to finish.
#[derive(Debug)]
pub struct ImportConflict {
    pub subrepo_name: String,
    pub pub_sha: String,
    pub conflicts: Vec<String>,
    pub state_path: PathBuf,
}

#[derive(Debug)]
pub enum ImportError {
    Conflict(ImportConflict),
    Git(GitError),
    Io(io::Error),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Conflict(c) => write!(
                f,
                "import of {} into {} conflicted",
                c.pub_sha, c.subrepo_name
            ),
            ImportError::Git(e) => e.fmt(f),
            ImportError::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<GitError> for ImportError {
    fn from(e: GitError) -> Self {
        ImportError::Git(e)
    }
}

impl From<io::Error> for ImportError {
    fn from(e: io::Error) -> Self {
        ImportError::Io(e)
    }
}

#[derive(Debug)]
pub struct ImportOutcome {
    pub imported: Vec<String>,
}

const STATE_FILE: &str = "pull-state.json";

fn state_dir(root: &Path) -> Result<PathBuf, GitError> {
    // `--git-dir` answers relative to the repo root for a normal checkout and absolute for a
    // linked worktree; joining onto `root` is right in both cases.
    let git_dir = git(root, &["rev-parse", "--git-dir"])?;
    Ok(root.join(git_dir).join("monosplice"))
}

pub fn sequencer_path(root: &Path) -> Result<PathBuf, GitError> {
    Ok(state_dir(root)?.join(STATE_FILE))
}

/// The interrupted import, or None when there is none (or the file cannot be understood).
pub fn read_sequencer(root: &Path) -> Option<PullSequencer> {
    let raw = fs::read_to_string(sequencer_path(root).ok()?).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_sequencer(root: &Path, state: &PullSequencer) -> Result<PathBuf, ImportError> {
    let dir = state_dir(root)?;
    fs::create_dir_all(&dir)?;
    let file = dir.join(STATE_FILE);
    // Same bytes the TypeScript CLI wrote: `JSON.stringify(state, null, 2)` plus a newline.
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&file, format!("{json}\n"))?;
    Ok(file)
}

pub fn clear_sequencer(root: &Path) {
    if let Ok(file) = sequencer_path(root) {
        let _ = fs::remove_file(file);
    }
}

/// Paths git reports as unmerged in the index, or [] when the merge is resolved.
pub fn unmerged_paths(root: &Path) -> Result<Vec<String>, GitError> {
    let out = git(root, &["diff", "--name-only", "--diff-filter=U"])?;
    if out.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(out.split('\n').map(str::to_string).collect())
    }
}

/// Import is the only operation that writes to the work tree and index, so it insists on
/// finding both pristine: anything staged would be swept into the import commit, and
/// anything modified under the subrepo would make `git apply --index` fail halfway.
///
/// `retry` is the command to retry, so `attach` does not tell the user to run `pull`.
pub fn check_import_preconditions(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    retry: &str,
) -> Option<String> {
    if rev_parse(root, "HEAD").is_none() {
        return Some(format!(
            "{} has no commits yet — commit something before importing from {}.",
            root.display(),
            subrepo.remote
        ));
    }
    let dirty = git(root, &["status", "--porcelain", "--", &subrepo.path]).unwrap_or_default();
    if !dirty.is_empty() {
        return Some(format!(
            "{}: {}/ has uncommitted changes:\n{dirty}\nCommit or stash them, then run `{retry}` again. Nothing was imported.",
            subrepo.name, subrepo.path
        ));
    }
    if !git_ok(root, &["diff", "--cached", "--quiet"]) {
        let staged = git(root, &["diff", "--cached", "--name-only"]).unwrap_or_default();
        return Some(format!(
            "{}: you have staged changes:\n{staged}\nAn import commits the index, so it would sweep them in. Commit or unstage them, then run `{retry}` again. Nothing was imported.",
            subrepo.name
        ));
    }
    None
}

/// First parent of a commit, or the empty tree for a root commit (also the snapshot case).
fn diff_base(root: &Path, sha: &str) -> Result<String, GitError> {
    let line = git(root, &["rev-list", "--parents", "-n", "1", sha])?;
    Ok(line
        .split(' ')
        .nth(1)
        .filter(|s| !s.is_empty())
        .unwrap_or(EMPTY_TREE)
        .to_string())
}

fn commit_import(root: &Path, c: &PullSequencerCommit) -> Result<String, GitError> {
    // --allow-empty: when the monorepo independently made the identical change the patch is
    // a no-op, but the commit (and its Origin trailer) is what marks the pub commit
    // reflected — skip it and push would refuse forever.
    // The standalone commit's own sync trailers describe the hop that published it (a
    // `Monosplice-Source` naming *its* monorepo); copied here they would read as this
    // monorepo's claims. Only the Origin this import writes is kept.
    let message = append_trailer(&strip_sync_trailers(&c.message), ORIGIN_TRAILER, &c.sha);
    // Author is the public commit's; the committer is whoever is running monosplice, which
    // is why it is left to the inherited environment.
    let env = [
        ("GIT_AUTHOR_NAME", c.author_name.clone()),
        ("GIT_AUTHOR_EMAIL", c.author_email.clone()),
        ("GIT_AUTHOR_DATE", c.author_date.clone()),
    ];
    git_with(
        root,
        &["commit", "--allow-empty", "--no-verify", "-m", &message],
        GitOpts {
            env: &env,
            input: None,
        },
    )?;
    git(root, &["rev-parse", "HEAD"])
}

fn exclude_warning(subrepo: &ResolvedSubrepo, rel_path: &str) -> String {
    format!(
        "warning: {}: imported {}/{rel_path}, but it matches an exclude pattern in your config.
The next `monosplice push {}` will DELETE it from {}.
Rename the file or drop the pattern from `exclude` if you want to keep it in the standalone repo.",
        subrepo.name, subrepo.path, subrepo.name, subrepo.remote
    )
}

/// Either the commit an import created, or the paths its three-way apply left unmerged.
enum ImportStep {
    Committed(String),
    Conflicts(Vec<String>),
}

/// Replay one standalone-repo commit onto the work tree. Returns the unmerged paths when the
/// three-way apply conflicted, or the monorepo commit it created when it applied.
fn import_one(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    meta: &PullSequencerCommit,
    warn: &mut dyn FnMut(String),
) -> Result<ImportStep, ImportError> {
    let base = diff_base(root, &meta.sha)?;
    let patch = git_buffer(
        root,
        &["diff-tree", "--binary", "-M", "-p", &base, &meta.sha],
        GitOpts::default(),
    )?;

    if !patch.is_empty() {
        let directory = format!("--directory={}", subrepo.path);
        // --3way merges concurrent monorepo edits instead of rejecting; the blobs it needs
        // are already local because loadSyncView fetched the public branch.
        let applied = git_with(
            root,
            &["apply", "--3way", "--index", &directory],
            GitOpts {
                env: &[],
                input: Some(&patch),
            },
        );
        if let Err(err) = applied {
            let conflicts = unmerged_paths(root)?;
            if conflicts.is_empty() {
                return Err(ImportError::Git(err));
            }
            return Ok(ImportStep::Conflicts(conflicts));
        }
    }

    let names = git(root, &["diff-tree", "--name-only", "-r", &base, &meta.sha])?;
    if !names.is_empty() && !subrepo.exclude.is_empty() {
        match make_excluder(&subrepo.exclude) {
            // A pattern too broken to compile costs the user a warning, never the import:
            // the work tree is already written by the time we get here.
            Err(detail) => warn(format!("warning: {}: {detail}", subrepo.name)),
            Ok(excluded) => {
                for rel in names.split('\n') {
                    if excluded.matches(rel) {
                        warn(exclude_warning(subrepo, rel));
                    }
                }
            }
        }
    }

    Ok(ImportStep::Committed(commit_import(root, meta)?))
}

fn read_sequencer_commit(root: &Path, sha: &str) -> Result<PullSequencerCommit, GitError> {
    let meta = read_commit(root, sha)?;
    Ok(PullSequencerCommit {
        sha: meta.sha,
        message: meta.message,
        author_name: meta.author_name,
        author_email: meta.author_email,
        author_date: meta.author_date,
    })
}

/// Replay standalone-repo commits (oldest first) into the monorepo, stopping at the first
/// conflict. `run` carries the provenance of an already-started pull across `--continue`; left
/// out, this call *is* the start of the run.
pub fn run_import(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    candidates: &[String],
    warn: &mut dyn FnMut(String),
    run: Option<RunProvenance>,
) -> Result<ImportOutcome, ImportError> {
    let (start_head, mut created) = match run {
        Some(r) => (r.start_head, r.created),
        None => (rev_parse(root, "HEAD").unwrap_or_default(), Vec::new()),
    };
    let mut imported: Vec<String> = Vec::new();
    for (idx, sha) in candidates.iter().enumerate() {
        let meta = read_sequencer_commit(root, sha)?;
        match import_one(root, subrepo, &meta, warn)? {
            ImportStep::Conflicts(conflicts) => {
                let state_path = write_sequencer(
                    root,
                    &PullSequencer {
                        subrepo: subrepo.name.clone(),
                        path: Some(subrepo.path.clone()),
                        current: meta,
                        remaining: candidates[idx + 1..].to_vec(),
                        start_head: Some(start_head),
                        created: Some(created),
                    },
                )?;
                return Err(ImportError::Conflict(ImportConflict {
                    subrepo_name: subrepo.name.clone(),
                    pub_sha: sha.clone(),
                    conflicts,
                    state_path,
                }));
            }
            ImportStep::Committed(mono_sha) => {
                created.push(mono_sha);
                imported.push(sha.clone());
            }
        }
    }
    Ok(ImportOutcome { imported })
}

/// Finish the commit the user just resolved, then carry on with what was left. A later
/// candidate can conflict too, which simply rewrites the sequencer — with the same run
/// provenance, so `--abort` after the second conflict still rewinds the whole pull.
pub fn continue_import(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    state: &PullSequencer,
    warn: &mut dyn FnMut(String),
) -> Result<ImportOutcome, ImportError> {
    let sha = commit_import(root, &state.current)?;
    clear_sequencer(root);
    let mut created = state.created.clone().unwrap_or_default();
    created.push(sha.clone());
    let run = RunProvenance {
        start_head: state.start_head.clone().unwrap_or(sha),
        created,
    };
    let rest = run_import(root, subrepo, &state.remaining, warn, Some(run))?;
    let mut imported = vec![state.current.sha.clone()];
    imported.extend(rest.imported);
    Ok(ImportOutcome { imported })
}

#[derive(Debug)]
pub struct AbortOutcome {
    /// True when the monorepo was rewound all the way to the pre-pull HEAD.
    pub rewound: bool,
    /// Monorepo commits this pull created and abort discarded (oldest first).
    pub discarded: Vec<String>,
    /// Commits this pull created that abort kept, because history moved after they landed.
    pub kept: Vec<String>,
    /// HEAD before the pull started, when the sequencer recorded it.
    pub start_head: Option<String>,
}

/// Are the commits between `start_head` and HEAD exactly the ones this pull run created?
/// That is the whole proof: anything else on the walk is somebody's work monosplice did not
/// make, and rewinding past it would destroy it.
fn run_owns_history(root: &Path, start_head: &str, created: &[String]) -> bool {
    if !git_ok(root, &["merge-base", "--is-ancestor", start_head, "HEAD"]) {
        return false;
    }
    let range = format!("{start_head}..HEAD");
    let Ok(walk) = rev_list(root, &[&range]) else {
        return false;
    };
    // The walk is newest-first, `created` oldest-first.
    walk.len() == created.len()
        && walk
            .iter()
            .enumerate()
            .all(|(i, sha)| *sha == created[created.len() - 1 - i])
}

/// Put the subrepo directory — and nothing else — back to how `target` has it: index first
/// (which also drops the conflict stages), then the work tree, then the files the aborted
/// import created, which are untracked by now. Import required the path to be pristine before
/// it started, so "untracked under the path" means "made by this pull".
fn restore_subrepo_path(root: &Path, sub_path: &str, target: &str) -> Result<(), GitError> {
    git(root, &["reset", "--quiet", target, "--", sub_path])?;
    // Nothing to check out when the path has no files at `target` and none in the index.
    git_ok(root, &["checkout", "--quiet", "--", sub_path]);
    git(root, &["clean", "-fdq", "--", sub_path])?;
    git(root, &["reset", "--quiet", "--soft", target])?;
    Ok(())
}

/// Abandon an interrupted import. Rewinds to the pre-pull HEAD when the sequencer can prove
/// every commit since is one this run made; otherwise it undoes only the conflicted step and
/// says which commits it left behind. Never touches anything outside the subrepo path.
pub fn abort_import(
    root: &Path,
    sub_path: &str,
    state: &PullSequencer,
) -> Result<AbortOutcome, GitError> {
    let head = git(root, &["rev-parse", "HEAD"])?;
    let created = state.created.clone().unwrap_or_default();
    let start_head = state.start_head.clone();
    let provable = match &start_head {
        Some(sh) => run_owns_history(root, sh, &created),
        None => false,
    };

    let target = if provable {
        start_head.clone().unwrap_or_else(|| head.clone())
    } else {
        head.clone()
    };
    restore_subrepo_path(root, sub_path, &target)?;
    clear_sequencer(root);

    Ok(AbortOutcome {
        rewound: provable,
        discarded: if provable {
            created.clone()
        } else {
            Vec::new()
        },
        kept: if provable { Vec::new() } else { created },
        start_head,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Same hermetic setup the e2e harness uses: no global or system git config in play.
    fn hermetic() {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
    }

    static DATE: AtomicU64 = AtomicU64::new(1_700_000_000);

    fn next_date() -> String {
        let ts = DATE.fetch_add(61, Ordering::SeqCst);
        format!("{ts} +0000")
    }

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(name: &str) -> Sandbox {
            hermetic();
            let dir =
                std::env::temp_dir().join(format!("ms-importer-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Sandbox(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sh(cwd: &Path, cmd: &str) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo with a fixed identity; commits made through `commit_as` get fixed dates.
    fn init_repo(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        fs::create_dir_all(&dir).unwrap();
        sh(&dir, "git init -q -b main .");
        sh(
            &dir,
            "git config user.name 'Mono Dev' && git config user.email mono@example.com",
        );
        dir
    }

    fn commit_as(dir: &Path, name: &str, email: &str, message: &str) -> String {
        let date = next_date();
        sh(
            dir,
            &format!(
                "git add -A && GIT_AUTHOR_NAME='{name}' GIT_AUTHOR_EMAIL='{email}' \
                 GIT_AUTHOR_DATE='{date}' GIT_COMMITTER_DATE='{date}' \
                 git commit -q --allow-empty -m '{message}'"
            ),
        );
        sh(dir, "git rev-parse HEAD")
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let file = dir.join(rel);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(file, content).unwrap();
    }

    fn read(dir: &Path, rel: &str) -> String {
        fs::read_to_string(dir.join(rel)).unwrap()
    }

    fn subrepo(exclude: &[&str]) -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: "core".to_string(),
            path: "core".to_string(),
            remote: "git@example.com:acme/core.git".to_string(),
            upstream: None,
            branch: "main".to_string(),
            push_branch: "main".to_string(),
            exclude: exclude.iter().map(|e| (*e).to_string()).collect(),
            rewrite_message: None,
            transform: None,
            scan: None,
            monorepo_id: None,
        }
    }

    /// Make the public repo's objects reachable from the monorepo, exactly as a pull's
    /// fetch would, and return its commits oldest-first.
    fn fetch_pub(mono: &Path, pub_repo: &Path) -> Vec<String> {
        sh(
            mono,
            &format!(
                "git fetch -q --no-tags {} +refs/heads/main:refs/monosplice/core/pub",
                pub_repo.display()
            ),
        );
        rev_list(mono, &["--reverse", "refs/monosplice/core/pub"]).unwrap()
    }

    fn collector() -> Vec<String> {
        Vec::new()
    }

    // ---- sequencer file ------------------------------------------------------------

    #[test]
    fn sequencer_path_lives_under_the_git_dir() {
        let sb = Sandbox::new("seq-path");
        let mono = init_repo(sb.path(), "mono");
        let p = sequencer_path(&mono).unwrap();
        assert_eq!(
            p,
            mono.join(".git").join("monosplice").join("pull-state.json")
        );
        // Nothing written yet.
        assert!(read_sequencer(&mono).is_none());
    }

    #[test]
    fn sequencer_round_trips_and_is_written_as_the_ts_wrote_it() {
        let sb = Sandbox::new("seq-roundtrip");
        let mono = init_repo(sb.path(), "mono");
        let state = PullSequencer {
            subrepo: "core".to_string(),
            path: Some("core".to_string()),
            current: PullSequencerCommit {
                sha: "aaa111".to_string(),
                message: "fix: thing\n".to_string(),
                author_name: "Pub Person".to_string(),
                author_email: "pub@example.com".to_string(),
                author_date: "1700000000 +0000".to_string(),
            },
            remaining: vec!["bbb222".to_string()],
            start_head: Some("head0".to_string()),
            created: Some(vec!["m1".to_string()]),
        };
        let file = write_sequencer(&mono, &state).unwrap();
        let raw = fs::read_to_string(&file).unwrap();
        assert!(raw.ends_with("}\n"), "trailing newline: {raw:?}");
        assert!(
            raw.contains("\n  \"subrepo\": \"core\","),
            "2-space indent: {raw}"
        );
        // camelCase spellings are the compatibility contract with the TS CLI.
        for key in [
            "\"authorName\"",
            "\"authorEmail\"",
            "\"authorDate\"",
            "\"startHead\"",
            "\"created\"",
            "\"remaining\"",
        ] {
            assert!(raw.contains(key), "missing {key} in {raw}");
        }

        let back = read_sequencer(&mono).expect("reads back");
        assert_eq!(back.subrepo, "core");
        assert_eq!(back.path.as_deref(), Some("core"));
        assert_eq!(back.current.sha, "aaa111");
        assert_eq!(back.current.author_name, "Pub Person");
        assert_eq!(back.current.author_date, "1700000000 +0000");
        assert_eq!(back.remaining, vec!["bbb222".to_string()]);
        assert_eq!(back.start_head.as_deref(), Some("head0"));
        assert_eq!(back.created, Some(vec!["m1".to_string()]));

        clear_sequencer(&mono);
        assert!(read_sequencer(&mono).is_none());
        // Clearing twice is not an error.
        clear_sequencer(&mono);
    }

    #[test]
    fn loads_a_sequencer_written_by_the_typescript_cli() {
        let sb = Sandbox::new("seq-ts");
        let mono = init_repo(sb.path(), "mono");
        let file = sequencer_path(&mono).unwrap();
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            r#"{
  "subrepo": "core",
  "path": "core",
  "current": {
    "sha": "cafe01",
    "message": "feat: x\n",
    "authorName": "P",
    "authorEmail": "p@x.y",
    "authorDate": "1700000000 +0000"
  },
  "remaining": [
    "beef02"
  ],
  "startHead": "h0",
  "created": [
    "m1",
    "m2"
  ]
}
"#,
        )
        .unwrap();
        let s = read_sequencer(&mono).expect("TS sequencer loads");
        assert_eq!(s.current.author_email, "p@x.y");
        assert_eq!(s.start_head.as_deref(), Some("h0"));
        assert_eq!(s.created.as_deref().map(<[String]>::len), Some(2));

        // A pre-provenance sequencer (no startHead/created) still loads.
        fs::write(
            &file,
            r#"{"subrepo":"core","current":{"sha":"a","message":"m","authorName":"n","authorEmail":"e","authorDate":"d"},"remaining":[]}"#,
        )
        .unwrap();
        let old = read_sequencer(&mono).expect("old sequencer loads");
        assert!(old.start_head.is_none() && old.created.is_none() && old.path.is_none());

        // Garbage is "no sequencer", never a panic.
        fs::write(&file, "not json").unwrap();
        assert!(read_sequencer(&mono).is_none());
    }

    // ---- preconditions -------------------------------------------------------------

    #[test]
    fn preconditions_refuse_a_repo_without_commits() {
        let sb = Sandbox::new("pre-empty");
        let mono = init_repo(sb.path(), "mono");
        let s = subrepo(&[]);
        let msg = check_import_preconditions(&mono, &s, "monosplice pull core").expect("refuses");
        assert!(msg.contains("has no commits yet"), "{msg}");
        assert!(msg.contains(&s.remote), "{msg}");
    }

    #[test]
    fn preconditions_refuse_a_dirty_subrepo_and_staged_changes() {
        let sb = Sandbox::new("pre-dirty");
        let mono = init_repo(sb.path(), "mono");
        write(&mono, "README.md", "root\n");
        write(&mono, "core/a.txt", "one\n");
        commit_as(&mono, "Mono Dev", "mono@example.com", "mono init");
        let s = subrepo(&[]);
        assert!(check_import_preconditions(&mono, &s, "monosplice pull core").is_none());

        write(&mono, "core/a.txt", "edited\n");
        let msg = check_import_preconditions(&mono, &s, "monosplice pull core").expect("refuses");
        assert!(msg.contains("core/ has uncommitted changes"), "{msg}");
        assert!(msg.contains("core/a.txt"), "{msg}");
        assert!(msg.contains("run `monosplice pull core` again"), "{msg}");
        assert!(msg.contains("Nothing was imported."), "{msg}");

        // Clean the subrepo, then stage something elsewhere.
        write(&mono, "core/a.txt", "one\n");
        write(&mono, "other.txt", "staged\n");
        sh(&mono, "git add other.txt");
        let msg = check_import_preconditions(&mono, &s, "monosplice attach core").expect("refuses");
        assert!(msg.contains("core: you have staged changes:"), "{msg}");
        assert!(msg.contains("other.txt"), "{msg}");
        assert!(msg.contains("An import commits the index"), "{msg}");
        // The retry command is the caller's, not a hardcoded `pull`.
        assert!(msg.contains("run `monosplice attach core` again"), "{msg}");
    }

    // ---- happy path ----------------------------------------------------------------

    #[test]
    fn imports_public_commits_preserving_message_and_author() {
        let sb = Sandbox::new("happy");
        let mono = init_repo(sb.path(), "mono");
        write(&mono, "README.md", "root\n");
        // The monorepo independently already has the file pub's second commit adds.
        write(&mono, "core/dup.txt", "same\n");
        let mono_start = commit_as(&mono, "Mono Dev", "mono@example.com", "mono init");

        let pubr = init_repo(sb.path(), "pub");
        write(&pubr, "hello.txt", "hello\n");
        let p1 = commit_as(&pubr, "Pub Person", "pub@example.com", "feat: add greeter");
        write(&pubr, "dup.txt", "same\n");
        let p2 = commit_as(&pubr, "Other Dev", "other@example.com", "feat: add dup");

        let candidates = fetch_pub(&mono, &pubr);
        assert_eq!(candidates, vec![p1.clone(), p2.clone()]);

        let mut warnings = collector();
        let s = subrepo(&[]);
        let out = run_import(&mono, &s, &candidates, &mut |m| warnings.push(m), None)
            .expect("imports cleanly");
        assert_eq!(out.imported, vec![p1.clone(), p2.clone()]);
        assert!(warnings.is_empty());

        // The patch landed under the subrepo directory, not at the root.
        assert_eq!(read(&mono, "core/hello.txt"), "hello\n");
        assert!(!mono.join("hello.txt").exists());

        let first = read_commit(&mono, "HEAD~1").unwrap();
        assert_eq!(
            first.message,
            format!("feat: add greeter\n\nMonosplice-Origin: {p1}\n")
        );
        assert_eq!(first.author_name, "Pub Person");
        assert_eq!(first.author_email, "pub@example.com");
        // Author identity — including the date — is the public commit's.
        assert_eq!(
            first.author_date,
            read_commit(&pubr, &p1).unwrap().author_date
        );
        // The committer is whoever ran monosplice.
        assert_eq!(first.committer_name, "Mono Dev");

        // --allow-empty: the monorepo already made this change, so the patch is a no-op,
        // but the commit still has to exist to mark the pub commit reflected.
        let second = read_commit(&mono, "HEAD").unwrap();
        assert_eq!(
            second.message,
            format!("feat: add dup\n\nMonosplice-Origin: {p2}\n")
        );
        assert!(
            git_ok(&mono, &["diff", "--quiet", "HEAD~1", "HEAD"]),
            "the second import should be an empty commit"
        );

        // Two new commits on top of where the run started, work tree clean, no sequencer.
        let walk = rev_list(&mono, &[&format!("{mono_start}..HEAD")]).unwrap();
        assert_eq!(walk.len(), 2);
        assert_eq!(git(&mono, &["status", "--porcelain"]).unwrap(), "");
        assert!(read_sequencer(&mono).is_none());
    }

    #[test]
    fn imports_a_root_commit_against_the_empty_tree() {
        let sb = Sandbox::new("root-commit");
        let mono = init_repo(sb.path(), "mono");
        write(&mono, "README.md", "root\n");
        commit_as(&mono, "Mono Dev", "mono@example.com", "mono init");

        let pubr = init_repo(sb.path(), "pub");
        write(&pubr, "a.txt", "a\n");
        let p1 = commit_as(&pubr, "Pub Person", "pub@example.com", "initial");
        assert_eq!(diff_base(&pubr, &p1).unwrap(), EMPTY_TREE);

        let candidates = fetch_pub(&mono, &pubr);
        let out = run_import(&mono, &subrepo(&[]), &candidates, &mut |_| {}, None).unwrap();
        assert_eq!(out.imported, vec![p1]);
        assert_eq!(read(&mono, "core/a.txt"), "a\n");
    }

    #[test]
    fn warns_about_imported_paths_the_config_excludes() {
        let sb = Sandbox::new("exclude-warn");
        let mono = init_repo(sb.path(), "mono");
        write(&mono, "README.md", "root\n");
        commit_as(&mono, "Mono Dev", "mono@example.com", "mono init");

        let pubr = init_repo(sb.path(), "pub");
        write(&pubr, "keep.txt", "keep\n");
        write(&pubr, "top.secret", "shh\n");
        commit_as(&pubr, "Pub Person", "pub@example.com", "add files");

        let candidates = fetch_pub(&mono, &pubr);
        let s = subrepo(&["**/*.secret"]);
        let mut warnings = collector();
        run_import(&mono, &s, &candidates, &mut |m| warnings.push(m), None).unwrap();

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            warnings[0],
            format!(
                "warning: core: imported core/top.secret, but it matches an exclude pattern in your config.
The next `monosplice push core` will DELETE it from {}.
Rename the file or drop the pattern from `exclude` if you want to keep it in the standalone repo.",
                s.remote
            )
        );
    }

    // ---- conflict / continue -------------------------------------------------------

    /// Public history that has diverged from a monorepo edit under the subrepo directory.
    /// Returns (mono, pub, candidates-after-the-shared-base, mono HEAD before the run).
    fn diverged_fixture(sb: &Sandbox, local_z: bool) -> (PathBuf, Vec<String>, String) {
        let mono = init_repo(sb.path(), "mono");
        write(&mono, "README.md", "root\n");
        write(&mono, "core/a.txt", "one\n");
        write(&mono, "core/z.txt", "zero\n");
        commit_as(&mono, "Mono Dev", "mono@example.com", "mono init");

        let pubr = init_repo(sb.path(), "pub");
        write(&pubr, "a.txt", "one\n");
        write(&pubr, "z.txt", "zero\n");
        commit_as(&pubr, "Pub Person", "pub@example.com", "pub init");
        write(&pubr, "a.txt", "two\n");
        let p2 = commit_as(&pubr, "Pub Person", "pub@example.com", "change a");
        write(&pubr, "z.txt", "one-zero\n");
        let p3 = commit_as(&pubr, "Pub Person", "pub@example.com", "change z");

        // The monorepo edited the same file(s) in the meantime.
        write(&mono, "core/a.txt", "local\n");
        if local_z {
            write(&mono, "core/z.txt", "local-z\n");
        }
        let start_head = commit_as(&mono, "Mono Dev", "mono@example.com", "local edit");

        fetch_pub(&mono, &pubr);
        (mono, vec![p2, p3], start_head)
    }

    #[test]
    fn a_conflict_writes_the_sequencer_and_continue_finishes_the_rest() {
        let sb = Sandbox::new("conflict-continue");
        let (mono, candidates, start_head) = diverged_fixture(&sb, false);
        let s = subrepo(&[]);

        let err = run_import(&mono, &s, &candidates, &mut |_| {}, None).unwrap_err();
        let ImportError::Conflict(c) = err else {
            panic!("expected a conflict, got {err:?}");
        };
        assert_eq!(c.subrepo_name, "core");
        assert_eq!(c.pub_sha, candidates[0]);
        assert_eq!(c.conflicts, vec!["core/a.txt".to_string()]);
        assert_eq!(c.state_path, sequencer_path(&mono).unwrap());
        assert!(c.state_path.exists());
        assert_eq!(
            ImportError::Conflict(ImportConflict {
                subrepo_name: "core".to_string(),
                pub_sha: candidates[0].clone(),
                conflicts: vec![],
                state_path: c.state_path.clone(),
            })
            .to_string(),
            format!("import of {} into core conflicted", candidates[0])
        );

        let state = read_sequencer(&mono).expect("sequencer on disk");
        assert_eq!(state.subrepo, "core");
        assert_eq!(state.path.as_deref(), Some("core"));
        assert_eq!(state.current.sha, candidates[0]);
        assert_eq!(state.current.author_name, "Pub Person");
        assert_eq!(state.remaining, vec![candidates[1].clone()]);
        assert_eq!(state.start_head.as_deref(), Some(start_head.as_str()));
        assert_eq!(state.created, Some(vec![]));
        // Nothing was committed: the conflicted step stops the run before its commit.
        assert_eq!(git(&mono, &["rev-parse", "HEAD"]).unwrap(), start_head);
        assert!(read(&mono, "core/a.txt").contains("<<<<<<<"));

        // The user resolves and continues.
        write(&mono, "core/a.txt", "resolved\n");
        sh(&mono, "git add core/a.txt");
        let out = continue_import(&mono, &s, &state, &mut |_| {}).expect("continues");
        assert_eq!(out.imported, candidates);

        assert!(read_sequencer(&mono).is_none());
        assert_eq!(read(&mono, "core/a.txt"), "resolved\n");
        assert_eq!(read(&mono, "core/z.txt"), "one-zero\n");
        let walk = rev_list(&mono, &[&format!("{start_head}..HEAD")]).unwrap();
        assert_eq!(walk.len(), 2);
        let resolved = read_commit(&mono, "HEAD~1").unwrap();
        assert_eq!(
            resolved.message,
            format!("change a\n\nMonosplice-Origin: {}\n", candidates[0])
        );
        assert_eq!(resolved.author_name, "Pub Person");
        assert_eq!(git(&mono, &["status", "--porcelain"]).unwrap(), "");
    }

    #[test]
    fn a_second_conflict_keeps_the_original_start_head_and_abort_rewinds_the_run() {
        let sb = Sandbox::new("conflict-twice");
        let (mono, candidates, start_head) = diverged_fixture(&sb, true);
        let s = subrepo(&[]);
        let a_before = read(&mono, "core/a.txt");
        let z_before = read(&mono, "core/z.txt");

        let err = run_import(&mono, &s, &candidates, &mut |_| {}, None).unwrap_err();
        assert!(matches!(err, ImportError::Conflict(_)));
        let first = read_sequencer(&mono).expect("sequencer");

        write(&mono, "core/a.txt", "resolved\n");
        sh(&mono, "git add core/a.txt");
        let err = continue_import(&mono, &s, &first, &mut |_| {}).unwrap_err();
        let ImportError::Conflict(c) = err else {
            panic!("expected a second conflict")
        };
        assert_eq!(c.pub_sha, candidates[1]);
        assert_eq!(c.conflicts, vec!["core/z.txt".to_string()]);

        let second = read_sequencer(&mono).expect("sequencer rewritten");
        // The provenance is the *run's*, not this step's: abort must still rewind everything.
        assert_eq!(second.start_head.as_deref(), Some(start_head.as_str()));
        let created = second.created.clone().expect("created recorded");
        assert_eq!(created.len(), 1);
        assert_eq!(created[0], git(&mono, &["rev-parse", "HEAD"]).unwrap());
        assert!(second.remaining.is_empty());

        // Abort: the whole run is provably ours, so it all goes.
        let out = abort_import(&mono, "core", &second).unwrap();
        assert!(out.rewound);
        assert_eq!(out.discarded, created);
        assert!(out.kept.is_empty());
        assert_eq!(out.start_head.as_deref(), Some(start_head.as_str()));

        assert_eq!(git(&mono, &["rev-parse", "HEAD"]).unwrap(), start_head);
        assert_eq!(read(&mono, "core/a.txt"), a_before);
        assert_eq!(read(&mono, "core/z.txt"), z_before);
        assert_eq!(git(&mono, &["status", "--porcelain"]).unwrap(), "");
        assert!(read_sequencer(&mono).is_none());
    }

    #[test]
    fn abort_keeps_history_it_cannot_prove_this_run_created() {
        let sb = Sandbox::new("abort-unprovable");
        let (mono, candidates, start_head) = diverged_fixture(&sb, true);
        let s = subrepo(&[]);

        let err = run_import(&mono, &s, &candidates, &mut |_| {}, None).unwrap_err();
        assert!(matches!(err, ImportError::Conflict(_)));
        let first = read_sequencer(&mono).expect("sequencer");
        write(&mono, "core/a.txt", "resolved\n");
        sh(&mono, "git add core/a.txt");
        let err = continue_import(&mono, &s, &first, &mut |_| {}).unwrap_err();
        assert!(matches!(err, ImportError::Conflict(_)));
        let state = read_sequencer(&mono).expect("sequencer");
        let created = state.created.clone().unwrap();

        // Somebody else's commit lands on top of the import (another worktree, a script,
        // a hook) — history moved, so rewinding past it would destroy work.
        let foreign = sh(
            &mono,
            "git commit-tree HEAD^{tree} -p HEAD -m 'someone else'",
        );
        sh(&mono, &format!("git update-ref HEAD {foreign}"));

        let out = abort_import(&mono, "core", &state).unwrap();
        assert!(!out.rewound);
        assert!(out.discarded.is_empty());
        assert_eq!(out.kept, created);
        assert_eq!(out.start_head.as_deref(), Some(start_head.as_str()));

        // History kept; only the conflicted step was undone.
        assert_eq!(git(&mono, &["rev-parse", "HEAD"]).unwrap(), foreign);
        assert_eq!(read(&mono, "core/a.txt"), "resolved\n");
        assert_eq!(read(&mono, "core/z.txt"), "local-z\n");
        assert_eq!(git(&mono, &["status", "--porcelain"]).unwrap(), "");
        assert!(read_sequencer(&mono).is_none());
    }

    #[test]
    fn abort_without_provenance_undoes_only_the_conflicted_step() {
        let sb = Sandbox::new("abort-no-provenance");
        let (mono, candidates, start_head) = diverged_fixture(&sb, false);
        let s = subrepo(&[]);
        let err = run_import(&mono, &s, &candidates, &mut |_| {}, None).unwrap_err();
        assert!(matches!(err, ImportError::Conflict(_)));

        // A sequencer from before the provenance fields existed.
        let mut state = read_sequencer(&mono).expect("sequencer");
        state.start_head = None;
        state.created = None;

        let out = abort_import(&mono, "core", &state).unwrap();
        assert!(!out.rewound);
        assert!(out.discarded.is_empty() && out.kept.is_empty());
        assert!(out.start_head.is_none());
        assert_eq!(git(&mono, &["rev-parse", "HEAD"]).unwrap(), start_head);
        assert_eq!(read(&mono, "core/a.txt"), "local\n");
        assert_eq!(git(&mono, &["status", "--porcelain"]).unwrap(), "");
    }

    #[test]
    fn abort_removes_files_the_aborted_import_created() {
        let sb = Sandbox::new("abort-untracked");
        let mono = init_repo(sb.path(), "mono");
        write(&mono, "README.md", "root\n");
        write(&mono, "core/a.txt", "one\n");
        commit_as(&mono, "Mono Dev", "mono@example.com", "mono init");

        let pubr = init_repo(sb.path(), "pub");
        write(&pubr, "a.txt", "one\n");
        commit_as(&pubr, "Pub Person", "pub@example.com", "pub init");
        // One commit that both adds a file and conflicts with the monorepo's own edit.
        write(&pubr, "a.txt", "two\n");
        write(&pubr, "brand-new.txt", "new\n");
        commit_as(&pubr, "Pub Person", "pub@example.com", "change a, add new");

        write(&mono, "core/a.txt", "local\n");
        let start_head = commit_as(&mono, "Mono Dev", "mono@example.com", "local edit");

        let candidates = fetch_pub(&mono, &pubr);
        let s = subrepo(&[]);
        let err = run_import(&mono, &s, &candidates[1..], &mut |_| {}, None).unwrap_err();
        assert!(matches!(err, ImportError::Conflict(_)));
        assert!(mono.join("core/brand-new.txt").exists());

        let state = read_sequencer(&mono).expect("sequencer");
        let out = abort_import(&mono, "core", &state).unwrap();
        assert!(out.rewound);
        assert_eq!(git(&mono, &["rev-parse", "HEAD"]).unwrap(), start_head);
        assert!(!mono.join("core/brand-new.txt").exists());
        assert_eq!(read(&mono, "core/a.txt"), "local\n");
        assert_eq!(git(&mono, &["status", "--porcelain"]).unwrap(), "");
    }

    #[test]
    fn unmerged_paths_is_empty_on_a_clean_index() {
        let sb = Sandbox::new("unmerged");
        let mono = init_repo(sb.path(), "mono");
        write(&mono, "README.md", "root\n");
        commit_as(&mono, "Mono Dev", "mono@example.com", "mono init");
        assert!(unmerged_paths(&mono).unwrap().is_empty());
    }
}
