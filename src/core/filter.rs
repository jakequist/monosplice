//! Port of `src/core/filter.ts` — what a monorepo commit actually publishes.
//!
//! Object-db only: the working tree and the index are never touched (CLAUDE.md).

use std::path::Path;

use crate::config::ResolvedSubrepo;
use crate::core::git::{build_tree, git, ls_tree_recursive, read_commit, GitError};
use crate::core::hooks::{materialize_tree, rehash_tree, run_tree_hook, MessageFile};
use crate::core::paths::make_excluder;

pub use crate::core::hooks::HookError;

/// Why a subtree could not be resolved. `Hook` is the one a caller reports on its own terms:
/// a rejected `scan` means the export stops before anything reaches a remote.
#[derive(Debug)]
pub enum FilterError {
    Hook(HookError),
    Git(GitError),
    Other(String),
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterError::Hook(e) => write!(f, "{e}"),
            FilterError::Git(e) => write!(f, "{e}"),
            FilterError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for FilterError {}

impl From<HookError> for FilterError {
    fn from(e: HookError) -> Self {
        FilterError::Hook(e)
    }
}

impl From<GitError> for FilterError {
    fn from(e: GitError) -> Self {
        FilterError::Git(e)
    }
}

/// Does the subrepo path hold any committed file at this revision? Deliberately unfiltered:
/// "is there anything here at all" must be answerable before excludes or hooks get a say,
/// so a broken hook can never masquerade as an empty directory.
pub fn has_committed_files(root: &Path, rev: &str, subrepo: &ResolvedSubrepo) -> bool {
    let treeish = format!("{rev}:{}", subrepo.path);
    let out = git(root, &["ls-tree", "-r", "--name-only", "-z", &treeish]).unwrap_or_default();
    !out.is_empty()
}

/// [`filtered_subtree`] used as an *identity* rather than as a publication: what tree does this
/// monorepo commit stand for on the public side?
///
/// The `scan` hook is deliberately dropped. It inspects a tree, it never shapes one, so whether
/// content passes a secret scan says nothing about which public tree the commit reproduces —
/// and a scan tightened after an export (a legacy secret, say) would otherwise veto every
/// answer, turning a comparison into a refusal. `transform` still runs, because it does shape
/// the tree. Callers that publish still go through [`filtered_subtree`], scan and all.
pub fn anchor_subtree(
    root: &Path,
    mono_commit: &str,
    subrepo: &ResolvedSubrepo,
) -> Result<Option<String>, FilterError> {
    let for_anchor = ResolvedSubrepo {
        scan: None,
        ..subrepo.clone()
    };
    filtered_subtree(root, mono_commit, &for_anchor)
}

/// Tree sha of `subrepo.path` at `mono_commit` after excludes and transform hooks, or `None`
/// when the path does not exist at that commit. Object-db only — the working tree and index
/// are never touched.
///
/// `rewrite-message` is deliberately not run here; it shapes the commit, not the tree, and
/// belongs to the exporter.
pub fn filtered_subtree(
    root: &Path,
    mono_commit: &str,
    subrepo: &ResolvedSubrepo,
) -> Result<Option<String>, FilterError> {
    let treeish = format!("{mono_commit}:{}", subrepo.path);
    let has_hooks = subrepo.transform.is_some() || subrepo.scan.is_some();

    if subrepo.exclude.is_empty() && !has_hooks {
        // `-d` keeps this a tree lookup: a missing path (or a path that is a file) yields
        // empty output rather than something we would splice into a commit as a tree.
        let out = git(
            root,
            &[
                "ls-tree",
                "-d",
                "--format=%(objectname)",
                mono_commit,
                "--",
                &subrepo.path,
            ],
        )
        .unwrap_or_default();
        return Ok(if out.is_empty() { None } else { Some(out) });
    }

    let Ok(entries) = ls_tree_recursive(root, &treeish) else {
        return Ok(None);
    };

    let excluded = make_excluder(&subrepo.exclude).map_err(FilterError::Other)?;
    let kept: Vec<_> = entries
        .into_iter()
        .filter(|e| !excluded.matches(&e.path))
        .collect();
    if !has_hooks {
        return Ok(Some(build_tree(root, &kept)?));
    }

    let meta = read_commit(root, mono_commit)?;

    // The hooks work on real files. The message file lives *outside* the materialized tree,
    // so a transform that sweeps up its whole working directory cannot publish it.
    let tree = materialize_tree(root, &kept).map_err(FilterError::Other)?;
    let message_file = MessageFile::new(&meta.message).map_err(FilterError::Other)?;

    // scan first: a rejection must stop the export before a transform gets to rewrite
    // anything, and before a single object is created.
    if let Some(cmd) = &subrepo.scan {
        run_tree_hook(
            "scan",
            cmd,
            tree.dir(),
            root,
            &subrepo.name,
            &meta.sha,
            message_file.path(),
        )?;
    }

    if let Some(cmd) = &subrepo.transform {
        run_tree_hook(
            "transform",
            cmd,
            tree.dir(),
            root,
            &subrepo.name,
            &meta.sha,
            message_file.path(),
        )?;
        // Gitlinks were never on disk, so the rehash cannot see them; they come back
        // untouched, exactly like the TS passthrough list.
        let mut rebuilt = tree.passthrough().to_vec();
        rebuilt.extend(rehash_tree(root, tree.dir()).map_err(FilterError::Other)?);
        return Ok(Some(build_tree(root, &rebuilt)?));
    }

    // scan-only: the tree is exactly what the excludes left behind.
    Ok(Some(build_tree(root, &kept)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn hermetic() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        });
    }

    struct Repo(PathBuf);

    impl Repo {
        fn new(tag: &str) -> Self {
            hermetic();
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "monosplice-filter-{tag}-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create repo dir");
            let r = Repo(dir);
            r.sh("git init -q -b main .");
            r.sh("git config user.name 'Mono Author' && git config user.email mono@example.test");
            r
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn sh(&self, cmd: &str) -> String {
            let out = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(&self.0)
                .env("GIT_AUTHOR_DATE", "1767225661 +0000")
                .env("GIT_COMMITTER_DATE", "1767225661 +0000")
                .output()
                .expect("spawn sh");
            assert!(
                out.status.success(),
                "{cmd}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn subrepo() -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: "core".to_string(),
            path: "core".to_string(),
            remote: "remote".to_string(),
            upstream: None,
            branch: "main".to_string(),
            push_branch: "main".to_string(),
            exclude: Vec::new(),
            rewrite_message: None,
            transform: None,
            scan: None,
        }
    }

    /// core/{README.md, .env, bin/run.sh (exec), link -> README.md} plus a top-level file
    /// outside the subrepo.
    fn fixture(tag: &str) -> Repo {
        let repo = Repo::new(tag);
        repo.sh("mkdir -p core/bin && printf 'hello\n' > core/README.md && printf 'SECRET=1\n' > core/.env && printf 'run\n' > core/bin/run.sh && chmod +x core/bin/run.sh && ln -s README.md core/link && printf 'top\n' > outside.txt");
        repo.sh("git add -A && git commit -q -m 'first commit'");
        repo
    }

    #[test]
    fn has_committed_files_answers_before_excludes_get_a_say() {
        let repo = fixture("has-files");
        let mut s = subrepo();
        assert!(has_committed_files(repo.path(), "HEAD", &s));
        // Even excluding everything, the raw question still says "yes, content is there".
        s.exclude = vec!["**/*".to_string()];
        assert!(has_committed_files(repo.path(), "HEAD", &s));
        s.path = "nope".to_string();
        assert!(!has_committed_files(repo.path(), "HEAD", &s));
    }

    #[test]
    fn the_fast_path_and_the_exclude_path_agree_when_nothing_matches() {
        let repo = fixture("fast-vs-slow");
        let fast = filtered_subtree(repo.path(), "HEAD", &subrepo())
            .expect("fast path")
            .expect("a tree");
        let mut s = subrepo();
        s.exclude = vec!["nothing-matches-this/**".to_string()];
        let slow = filtered_subtree(repo.path(), "HEAD", &s)
            .expect("exclude path")
            .expect("a tree");
        assert_eq!(fast, slow);
        assert_eq!(fast, repo.sh("git rev-parse HEAD:core"));
    }

    #[test]
    fn an_exclude_drops_a_file_from_the_published_tree() {
        let repo = fixture("exclude");
        let mut s = subrepo();
        s.exclude = vec![".env".to_string()];
        let tree = filtered_subtree(repo.path(), "HEAD", &s)
            .expect("exclude path")
            .expect("a tree");
        let listing = repo.sh(&format!("git ls-tree -r --name-only {tree}"));
        assert!(!listing.contains(".env"), "{listing}");
        assert!(listing.contains("README.md"), "{listing}");
        assert_ne!(tree, repo.sh("git rev-parse HEAD:core"));
    }

    /// The identity question ("which public tree does this commit stand for?") must survive a
    /// scan hook that rejects the content, because a scan judges content and never shapes it.
    #[test]
    fn an_anchor_subtree_ignores_a_rejecting_scan_but_still_honours_excludes() {
        let repo = fixture("anchor-subtree");
        let mut s = subrepo();
        s.exclude = vec![".env".to_string()];
        let published = filtered_subtree(repo.path(), "HEAD", &s)
            .expect("exclude path")
            .expect("a tree");

        s.scan = Some("exit 1".to_string());
        assert!(
            filtered_subtree(repo.path(), "HEAD", &s).is_err(),
            "publishing still runs the scan"
        );
        assert_eq!(
            anchor_subtree(repo.path(), "HEAD", &s).expect("no scan, no rejection"),
            Some(published),
            "...and the exclude still applies"
        );
    }

    #[test]
    fn a_missing_path_is_none_on_both_paths() {
        let repo = fixture("missing");
        let mut s = subrepo();
        s.path = "does-not-exist".to_string();
        assert_eq!(filtered_subtree(repo.path(), "HEAD", &s).expect("ok"), None);
        s.exclude = vec!["x".to_string()];
        assert_eq!(filtered_subtree(repo.path(), "HEAD", &s).expect("ok"), None);
    }

    #[test]
    fn a_path_that_is_a_file_is_none_on_both_paths() {
        let repo = fixture("file-not-dir");
        let mut s = subrepo();
        s.path = "outside.txt".to_string();
        assert_eq!(filtered_subtree(repo.path(), "HEAD", &s).expect("ok"), None);
        s.exclude = vec!["x".to_string()];
        assert_eq!(filtered_subtree(repo.path(), "HEAD", &s).expect("ok"), None);
    }

    #[test]
    fn a_rejecting_scan_hook_surfaces_a_hook_error_with_its_stderr() {
        let repo = fixture("scan-reject");
        let mut s = subrepo();
        s.scan = Some(
            "if grep -rq SECRET .; then echo 'secret found in core' 1>&2; exit 1; fi".to_string(),
        );
        let err = filtered_subtree(repo.path(), "HEAD", &s).expect_err("scan rejects");
        let FilterError::Hook(hook) = err else {
            panic!("expected a hook error, got {err}")
        };
        assert_eq!(hook.hook, "scan");
        assert_eq!(hook.subrepo, "core");
        assert_eq!(hook.detail, "secret found in core");
        assert_eq!(hook.mono_sha, repo.sh("git rev-parse HEAD"));

        // ...and the same scan passes once the offending file is excluded, proving the hook
        // sees the post-exclude tree.
        s.exclude = vec![".env".to_string()];
        assert!(filtered_subtree(repo.path(), "HEAD", &s).is_ok());
    }

    #[test]
    fn a_transform_that_rewrites_a_file_changes_the_exported_tree() {
        let repo = fixture("transform");
        let mut s = subrepo();
        s.transform = Some("printf 'rewritten\n' > README.md".to_string());
        let tree = filtered_subtree(repo.path(), "HEAD", &s)
            .expect("transform runs")
            .expect("a tree");
        assert_ne!(tree, repo.sh("git rev-parse HEAD:core"));
        assert_eq!(
            repo.sh(&format!("git cat-file blob {tree}:README.md")),
            "rewritten"
        );
        // Untouched neighbours survive, exec bit and all.
        assert_eq!(
            repo.sh(&format!("git cat-file blob {tree}:bin/run.sh")),
            "run"
        );
        let listing = repo.sh(&format!("git ls-tree -r {tree}"));
        assert!(listing.contains("100755 blob"), "{listing}");
    }

    #[test]
    fn a_transform_may_add_a_dotfile_and_it_is_kept() {
        let repo = fixture("transform-dotfile");
        let mut s = subrepo();
        s.transform = Some(
            "printf 'ok\n' > .added && mkdir -p deep && printf 'd\n' > deep/.also".to_string(),
        );
        let tree = filtered_subtree(repo.path(), "HEAD", &s)
            .expect("transform runs")
            .expect("a tree");
        assert_eq!(repo.sh(&format!("git cat-file blob {tree}:.added")), "ok");
        assert_eq!(
            repo.sh(&format!("git cat-file blob {tree}:deep/.also")),
            "d"
        );
    }

    #[test]
    fn a_symlink_round_trips_through_materialize_and_rehash() {
        let repo = fixture("symlink");
        let mut s = subrepo();
        // A transform that changes nothing must reproduce the original tree exactly.
        s.transform = Some("true".to_string());
        let tree = filtered_subtree(repo.path(), "HEAD", &s)
            .expect("transform runs")
            .expect("a tree");
        assert_eq!(tree, repo.sh("git rev-parse HEAD:core"));
        let listing = repo.sh(&format!("git ls-tree {tree} link"));
        assert!(listing.starts_with("120000 blob"), "{listing}");
    }

    #[test]
    fn a_failing_transform_hook_aborts_with_its_stderr() {
        let repo = fixture("transform-fail");
        let mut s = subrepo();
        s.transform = Some("echo 'formatter blew up' 1>&2; exit 4".to_string());
        let err = filtered_subtree(repo.path(), "HEAD", &s).expect_err("transform fails");
        let FilterError::Hook(hook) = err else {
            panic!("expected a hook error")
        };
        assert_eq!(hook.hook, "transform");
        assert_eq!(hook.detail, "formatter blew up");
    }

    #[test]
    fn hooks_run_in_the_materialized_dir_with_the_monosplice_env_visible() {
        let repo = fixture("hook-cwd");
        let mono_sha = repo.sh("git rev-parse HEAD");
        let mut s = subrepo();
        // The hook writes what it sees; the assertions read it back out of the exported tree.
        s.transform = Some(
            "printf '%s\n%s\n' \"$MONOSPLICE_SUBREPO\" \"$MONOSPLICE_MONO_SHA\" > seen.txt; ls > listing.txt; test -f README.md || exit 9; head -1 \"$MONOSPLICE_MESSAGE_FILE\" > message.txt"
                .to_string(),
        );
        let tree = filtered_subtree(repo.path(), "HEAD", &s)
            .expect("transform runs")
            .expect("a tree");
        assert_eq!(
            repo.sh(&format!("git cat-file blob {tree}:seen.txt")),
            format!("core\n{mono_sha}")
        );
        // cwd is the materialized tree, not the monorepo root: `outside.txt` is not there.
        let listing = repo.sh(&format!("git cat-file blob {tree}:listing.txt"));
        assert!(listing.contains("README.md"), "{listing}");
        assert!(!listing.contains("outside.txt"), "{listing}");
        assert_eq!(
            repo.sh(&format!("git cat-file blob {tree}:message.txt")),
            "first commit"
        );
    }

    #[test]
    fn a_gitlink_survives_a_transform_untouched() {
        let repo = fixture("gitlink");
        // A real submodule entry, built with plumbing so no network or nested repo is needed.
        let base = repo.sh("git rev-parse HEAD:core");
        let gitlink_sha = repo.sh("git rev-parse HEAD");
        let tree_with_link = repo.sh(&format!(
            "{{ git ls-tree {base}; printf '160000 commit {gitlink_sha}\\tvendor\\n'; }} | git mktree"
        ));
        let commit = repo.sh(&format!(
            "git commit-tree {tree_with_link} -p HEAD -m 'add submodule' < /dev/null"
        ));
        // Splice it in as the subrepo path of a fresh mono commit.
        let mono_tree = repo.sh(&format!(
            "printf '040000 tree {tree_with_link}\\tcore\\n' | git mktree"
        ));
        let mono = repo.sh(&format!(
            "git commit-tree {mono_tree} -p {commit} -m 'with submodule' < /dev/null"
        ));

        let mut s = subrepo();
        s.transform = Some("printf 'changed\n' > README.md".to_string());
        let tree = filtered_subtree(repo.path(), &mono, &s)
            .expect("transform runs")
            .expect("a tree");
        let listing = repo.sh(&format!("git ls-tree {tree} vendor"));
        assert_eq!(listing, format!("160000 commit {gitlink_sha}\tvendor"));
    }
}
