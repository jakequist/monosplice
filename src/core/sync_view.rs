//! Port of `src/core/sync.ts` — every sync cursor, derived from trailers on each run.
//!
//! There is no authoritative state file (CLAUDE.md): the mapping between the monorepo and a
//! public repo lives entirely in `Monosplice-Source` / `Monosplice-Origin` trailers, and this
//! module re-derives it from scratch every time.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::ResolvedSubrepo;
use crate::core::filter::anchor_subtree;
use crate::core::git::{
    existing_commits, fetch_branch, git, git_with, ls_remote_branch, missing_objects, rev_list,
    rev_parse, trailer_values, GitError, GitOpts,
};
use crate::core::trailers::{ORIGIN_TRAILER, SOURCE_TRAILER};

/// Where a subrepo's public branch is mirrored inside the monorepo's object db.
pub fn remote_tracking_ref(name: &str) -> String {
    format!("refs/monosplice/{name}/remote")
}

/// Where the fork's push branch is mirrored (triangular mode only).
pub fn fork_tracking_ref(name: &str) -> String {
    format!("refs/monosplice/{name}/fork")
}

/// The repository every sync decision is made against. With `upstream` configured that is
/// upstream and only upstream: the fork is a derived artifact monosplice rebuilds, so
/// consulting it for imports or anchors would let our own exports masquerade as public
/// history.
pub fn pull_source(s: &ResolvedSubrepo) -> &str {
    s.upstream.as_deref().unwrap_or(&s.remote)
}

/// Is this subrepo pulled from one repository and pushed to another?
pub fn is_triangular(s: &ResolvedSubrepo) -> bool {
    s.upstream.is_some()
}

/// How much of the network a view may use.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncViewOptions {
    /// Skip every fetch and derive the view from the remote-tracking refs already on disk.
    pub offline: bool,
}

/// Why a view could not be derived.
#[derive(Debug)]
pub enum SyncViewError {
    /// Offline, and this subrepo has never been fetched. There is no honest answer — an
    /// absent tracking ref is indistinguishable from a remote with no branch — so the caller
    /// reports the gap instead of guessing at counts.
    NoFetchYet {
        subrepo: String,
    },
    Git(GitError),
}

impl std::fmt::Display for SyncViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncViewError::NoFetchYet { subrepo } => {
                write!(f, "{subrepo}: no fetch yet — run without --offline first")
            }
            SyncViewError::Git(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SyncViewError {}

impl From<GitError> for SyncViewError {
    fn from(e: GitError) -> Self {
        SyncViewError::Git(e)
    }
}

/// What the fork's push branch looks like right now. Triangular mode only.
#[derive(Debug, Clone)]
pub struct ForkState {
    /// Fork branch head, or `None` when the fork does not have that branch yet.
    pub head: Option<String>,
}

/// A public commit claiming to export a monorepo commit that this clone does not have.
#[derive(Debug, Clone)]
pub struct BrokenSourceRef {
    pub pub_sha: String,
    pub mono_sha: String,
}

#[derive(Debug, Clone)]
pub struct SyncView {
    /// Local ref mirroring the public branch.
    pub tracking_ref: String,
    /// Public branch head, or `None` when the remote branch does not exist yet.
    pub pub_head: Option<String>,
    /// monorepo sha -> public sha, derived from `Monosplice-Source` trailers in pub history.
    pub exported_mono_to_pub: HashMap<String, String>,
    /// Public shas already imported into the monorepo, from `Monosplice-Origin` trailers on HEAD.
    pub imported_pub_shas: HashSet<String>,
    /// Where the export scan starts: the newest commit on the HEAD walk that is either already
    /// exported (`Monosplice-Source` names it) or anchors the monorepo to the public branch
    /// (`Monosplice-Origin` naming pub head or one of its ancestors). Export scans
    /// `export_base..HEAD`; `None` means "scan all of HEAD" (nothing published yet).
    pub export_base: Option<String>,
    /// Newest monorepo commit that pub history claims to have exported and that still exists
    /// locally. Not the scan base — its job is rewrite detection: a commit that was rebased
    /// away lives on in the reflog but is absent from the HEAD walk, so `export_base` cannot
    /// see it.
    pub last_exported_mono: Option<String>,
    /// Public commits that are neither our exports nor already reflected (oldest first).
    pub unreflected_pub: Vec<String>,
    /// `Monosplice-Source` trailers naming monorepo commits this clone does not have, found on
    /// or above the newest public commit whose trailer *does* resolve. Nothing above the
    /// mapping's newest readable point can be checked, so export refuses while any exist.
    pub broken_source_refs: Vec<BrokenSourceRef>,
    /// The same, but *behind* the newest resolvable anchor: history, not the mapping.
    ///
    /// A rebase on one machine rewrites the sha an earlier export recorded, and every clone made
    /// afterwards is missing it forever. Once a newer public commit names a commit this clone
    /// has, that anchor decides what is published and the dead trailers below it can no longer
    /// change the answer — they are reported and never refused.
    pub superseded_source_refs: Vec<BrokenSourceRef>,
    /// Do the two repos know about each other at all? False means first contact: the public
    /// branch has history, but nothing on either side references the other, so the only safe
    /// move is `monosplice attach`.
    pub related: bool,
}

/// The view of a subrepo whose public branch does not exist yet.
pub fn unpublished_view(name: &str) -> SyncView {
    SyncView {
        tracking_ref: remote_tracking_ref(name),
        pub_head: None,
        exported_mono_to_pub: HashMap::new(),
        imported_pub_shas: HashSet::new(),
        export_base: None,
        last_exported_mono: None,
        unreflected_pub: Vec::new(),
        broken_source_refs: Vec::new(),
        superseded_source_refs: Vec::new(),
        related: false,
    }
}

/// Mirror the fork's push branch locally. `ls_remote_branch` first, exactly as
/// `load_sync_view` does, so an unreachable fork raises a GitError the caller can attribute to
/// the fork rather than a fetch failure that reads like the branch is missing.
pub fn load_fork_state(
    root: &Path,
    s: &ResolvedSubrepo,
    opts: &SyncViewOptions,
) -> Result<ForkState, GitError> {
    if opts.offline {
        return Ok(ForkState {
            head: rev_parse(root, &fork_tracking_ref(&s.name)),
        });
    }
    let Some(head) = ls_remote_branch(root, &s.remote, &s.push_branch)? else {
        return Ok(ForkState { head: None });
    };
    fetch_branch(root, &s.remote, &s.push_branch, &fork_tracking_ref(&s.name))?;
    Ok(ForkState { head: Some(head) })
}

/// Fork state for reporting: an unreachable fork is a note, not a crash.
pub fn try_load_fork_state(
    root: &Path,
    s: &ResolvedSubrepo,
    opts: &SyncViewOptions,
) -> (Option<ForkState>, Option<GitError>) {
    match load_fork_state(root, s, opts) {
        Ok(state) => (Some(state), None),
        Err(e) => (None, Some(e)),
    }
}

/// Does this monorepo commit reproduce, exactly, the public commit it claims to reflect?
/// An attach anchor commit and a clean import do; a *conflicted* import and an import of a file
/// the config excludes do not — they carry work the public branch has never seen, so they
/// cannot be an export boundary.
///
/// The comparison runs through [`anchor_subtree`], so the `scan` hook is dropped: a scan that
/// rejects already-published content (a legacy secret, a hook tightened after the attach) must
/// not veto anchor detection — a vetoed anchor collapses `export_base` to "scan all of HEAD",
/// which re-exports every ancestor of the anchor. If a `transform` cannot run, the commit
/// genuinely is not a boundary and `push` reports the failure on its own terms.
fn reflects_exactly(root: &Path, s: &ResolvedSubrepo, mono_sha: &str, pub_sha: &str) -> bool {
    let Ok(Some(mono_tree)) = anchor_subtree(root, mono_sha, s) else {
        return false;
    };
    match git(root, &["rev-parse", &format!("{pub_sha}^{{tree}}")]) {
        Ok(pub_tree) => mono_tree == pub_tree,
        Err(_) => false,
    }
}

/// Walk monorepo history from HEAD and stop at the first commit whose publishable subtree the
/// public branch already contains. Two ways to qualify: pub says it exported this commit
/// (`Monosplice-Source`), or the commit imported public work and reproduces it exactly
/// (`Monosplice-Origin`) — the second is what stops a `push` right after an `attach` from
/// replaying the monorepo's entire pre-attach history onto the newly connected repo.
///
/// One `rev-list` for the walk, then O(1) lookups: both trailer maps are already in hand and
/// `pub_ancestors` is the pub-side walk this function's caller needed anyway, so an Origin
/// candidate costs a set probe rather than a `merge-base` process.
fn find_export_anchor(
    root: &Path,
    s: &ResolvedSubrepo,
    exported_mono_to_pub: &HashMap<String, String>,
    origin_by_mono: &HashMap<String, Vec<String>>,
    pub_ancestors: &HashSet<String>,
) -> Result<(Option<String>, bool), GitError> {
    if exported_mono_to_pub.is_empty() && origin_by_mono.is_empty() {
        return Ok((None, false));
    }

    let mut related = !exported_mono_to_pub.is_empty();
    for mono_sha in rev_list(root, &["HEAD"])? {
        if exported_mono_to_pub.contains_key(&mono_sha) {
            return Ok((Some(mono_sha), true));
        }
        for pub_sha in origin_by_mono
            .get(&mono_sha)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if !pub_ancestors.contains(pub_sha) {
                continue;
            }
            related = true;
            if reflects_exactly(root, s, &mono_sha, pub_sha) {
                return Ok((Some(mono_sha), true));
            }
        }
    }
    Ok((None, related))
}

/// Public commits the monorepo has not seen. Ancestry, not per-commit bookkeeping: a shallow
/// snapshot `attach` records only the pub head as imported, and every ancestor of a reflected
/// commit is reflected by construction. Our own exports drop out by trailer.
fn find_unreflected_pub(
    root: &Path,
    tracking_ref: &str,
    imported_pub_shas: &HashSet<String>,
    source_by_pub: &HashMap<String, Vec<String>>,
) -> Result<Vec<String>, GitError> {
    // A forged or force-pushed-away Origin value would abort the whole rev-list, so only
    // values that resolve to a commit here are allowed to negate anything.
    let candidates: Vec<String> = imported_pub_shas.iter().cloned().collect();
    let reflected = existing_commits(root, &candidates)?;
    let out = if reflected.is_empty() {
        git(root, &["rev-list", "--reverse", tracking_ref])?
    } else {
        // --stdin instead of argv: pub histories can carry thousands of reflected commits.
        let input: String = reflected.iter().map(|sha| format!("^{sha}\n")).collect();
        git_with(
            root,
            &["rev-list", "--reverse", tracking_ref, "--stdin"],
            GitOpts {
                input: Some(input.as_bytes()),
                ..Default::default()
            },
        )?
    };
    if out.is_empty() {
        return Ok(Vec::new());
    }
    Ok(out
        .split('\n')
        .filter(|sha| !source_by_pub.contains_key(*sha))
        .map(str::to_string)
        .collect())
}

/// Derive every sync cursor from trailers. There is no state file: this runs on each
/// invocation. `ls_remote_branch` goes first so an unreachable remote fails with a GitError
/// carrying git's own stderr, and a missing branch is reported as "not published yet" rather
/// than as a confusing fetch failure.
pub fn load_sync_view(
    root: &Path,
    s: &ResolvedSubrepo,
    opts: &SyncViewOptions,
) -> Result<SyncView, SyncViewError> {
    let tracking_ref = remote_tracking_ref(&s.name);
    let source = pull_source(s);
    let pub_head = if opts.offline {
        rev_parse(root, &tracking_ref)
    } else {
        ls_remote_branch(root, source, &s.branch)?
    };

    let origin_by_mono = if rev_parse(root, "HEAD").is_some() {
        trailer_values(root, ORIGIN_TRAILER, &["HEAD"])?
    } else {
        HashMap::new()
    };
    let mut imported_pub_shas: HashSet<String> = HashSet::new();
    for values in origin_by_mono.values() {
        for v in values {
            imported_pub_shas.insert(v.clone());
        }
    }

    let Some(pub_head) = pub_head else {
        if opts.offline {
            return Err(SyncViewError::NoFetchYet {
                subrepo: s.name.clone(),
            });
        }
        return Ok(SyncView {
            imported_pub_shas,
            ..unpublished_view(&s.name)
        });
    };

    if !opts.offline {
        fetch_branch(root, source, &s.branch, &tracking_ref)?;
    }

    let source_by_pub = trailer_values(root, SOURCE_TRAILER, &[&tracking_ref])?;

    // The pub walk drives both the mapping and the broken-ref scan. Newest first, so the
    // newest public commit claiming a monorepo commit is the one the mapping records — the
    // insertion order of the TS `Map`, made explicit because a Rust HashMap has none.
    let pub_walk = rev_list(root, &[&tracking_ref])?;
    let mut exported_mono_to_pub: HashMap<String, String> = HashMap::new();
    let mut exported_mono_order: Vec<String> = Vec::new();
    for pub_sha in &pub_walk {
        let Some(values) = source_by_pub.get(pub_sha) else {
            continue;
        };
        for mono_sha in values {
            if !exported_mono_to_pub.contains_key(mono_sha) {
                exported_mono_to_pub.insert(mono_sha.clone(), pub_sha.clone());
                exported_mono_order.push(mono_sha.clone());
            }
        }
    }

    let missing = missing_objects(root, &exported_mono_order)?;
    let mut broken_source_refs: Vec<BrokenSourceRef> = Vec::new();
    let mut superseded_source_refs: Vec<BrokenSourceRef> = Vec::new();

    // Newest first, and validation stops at the newest trailer that resolves: that commit is
    // where the mapping is still readable, and everything below it is already published by
    // construction. A dead trailer above it (or before any resolve) leaves a stretch of public
    // history whose provenance cannot be checked at all — that is the broken mapping. A dead
    // trailer below it is the fossil of a rewrite that a later export already superseded.
    let mut pub_ancestors: HashSet<String> = HashSet::new();
    let mut last_exported_mono: Option<String> = None;
    for pub_sha in &pub_walk {
        pub_ancestors.insert(pub_sha.clone());
        let Some(values) = source_by_pub.get(pub_sha) else {
            continue;
        };
        for mono_sha in values {
            if missing.contains(mono_sha) {
                let dead = BrokenSourceRef {
                    pub_sha: pub_sha.clone(),
                    mono_sha: mono_sha.clone(),
                };
                if last_exported_mono.is_some() {
                    superseded_source_refs.push(dead);
                } else {
                    broken_source_refs.push(dead);
                }
            } else if last_exported_mono.is_none() {
                last_exported_mono = Some(mono_sha.clone());
            }
        }
    }

    let (export_base, related) = find_export_anchor(
        root,
        s,
        &exported_mono_to_pub,
        &origin_by_mono,
        &pub_ancestors,
    )?;

    let unreflected_pub =
        find_unreflected_pub(root, &tracking_ref, &imported_pub_shas, &source_by_pub)?;

    Ok(SyncView {
        tracking_ref,
        pub_head: Some(pub_head),
        exported_mono_to_pub,
        imported_pub_shas,
        export_base,
        last_exported_mono,
        unreflected_pub,
        broken_source_refs,
        superseded_source_refs,
        related,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn subrepo(upstream: Option<&str>) -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: "core".to_string(),
            path: "core".to_string(),
            remote: "fork".to_string(),
            upstream: upstream.map(str::to_string),
            branch: "main".to_string(),
            push_branch: "main".to_string(),
            exclude: Vec::new(),
            rewrite_message: None,
            transform: None,
            scan: None,
        }
    }

    #[test]
    fn tracking_refs_are_namespaced_per_subrepo() {
        assert_eq!(remote_tracking_ref("core"), "refs/monosplice/core/remote");
        assert_eq!(fork_tracking_ref("core"), "refs/monosplice/core/fork");
    }

    #[test]
    fn upstream_decides_where_the_tree_comes_from() {
        assert_eq!(pull_source(&subrepo(None)), "fork");
        assert!(!is_triangular(&subrepo(None)));
        assert_eq!(pull_source(&subrepo(Some("up"))), "up");
        assert!(is_triangular(&subrepo(Some("up"))));
    }

    #[test]
    fn an_unpublished_view_knows_nothing_and_is_unrelated() {
        let view = unpublished_view("core");
        assert_eq!(view.tracking_ref, "refs/monosplice/core/remote");
        assert_eq!(view.pub_head, None);
        assert!(view.exported_mono_to_pub.is_empty());
        assert!(view.imported_pub_shas.is_empty());
        assert_eq!(view.export_base, None);
        assert_eq!(view.last_exported_mono, None);
        assert!(view.unreflected_pub.is_empty());
        assert!(view.broken_source_refs.is_empty());
        assert!(!view.related);
    }

    #[test]
    fn no_fetch_yet_reads_like_the_ts_error() {
        let err = SyncViewError::NoFetchYet {
            subrepo: "core".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "core: no fetch yet — run without --offline first"
        );
    }

    // --- fixtures ---

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn hermetic() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        });
    }

    /// A monorepo with `core/`, plus a bare "public" remote for it.
    struct Fixture {
        dir: PathBuf,
        mono: PathBuf,
        remote: PathBuf,
        dates: AtomicU64,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            hermetic();
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "monosplice-syncview-{tag}-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create fixture dir");
            let f = Fixture {
                mono: dir.join("mono"),
                remote: dir.join("pub.git"),
                dir,
                dates: AtomicU64::new(0),
            };
            fs::create_dir_all(&f.mono).unwrap();
            sh(
                &f.dir,
                &format!("git init -q --bare {}", f.remote.display()),
                0,
            );
            f.sh("git init -q -b main .");
            f.sh("git config user.name 'Mono Author' && git config user.email mono@example.test");
            f.sh("mkdir -p core && printf 'hello\n' > core/README.md && printf 'top\n' > top.txt");
            f.commit("first commit");
            f
        }

        fn root(&self) -> &Path {
            &self.mono
        }

        fn remote_url(&self) -> String {
            self.remote.display().to_string()
        }

        fn subrepo(&self) -> ResolvedSubrepo {
            let mut s = subrepo(None);
            s.remote = self.remote_url();
            s
        }

        fn next_date(&self) -> u64 {
            1_767_225_600 + (self.dates.fetch_add(1, Ordering::SeqCst) + 1) * 61
        }

        fn sh(&self, cmd: &str) -> String {
            sh(&self.mono, cmd, self.next_date())
        }

        fn commit(&self, message: &str) -> String {
            self.sh(&format!(
                "git add -A && git commit -q --allow-empty -m {}",
                shq(message)
            ));
            self.sh("git rev-parse HEAD")
        }

        /// A public commit built with plumbing (no clone, no working tree), pushed to the
        /// bare remote's branch.
        fn push_pub(&self, tree: &str, parent: Option<&str>, message: &str) -> String {
            let parent = match parent {
                Some(p) => format!("-p {p}"),
                None => String::new(),
            };
            let sha = self.sh(&format!(
                "printf %s {} | git commit-tree {tree} {parent}",
                shq(message)
            ));
            self.sh(&format!(
                "git push -q --force {} {sha}:refs/heads/main",
                self.remote_url()
            ));
            sha
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn shq(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    fn sh(cwd: &Path, cmd: &str, date: u64) -> String {
        let stamp = format!("{date} +0000");
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .env("GIT_AUTHOR_DATE", &stamp)
            .env("GIT_COMMITTER_DATE", &stamp)
            .output()
            .expect("spawn sh");
        assert!(
            out.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn online() -> SyncViewOptions {
        SyncViewOptions { offline: false }
    }

    // --- load_sync_view ---

    #[test]
    fn an_unpublished_remote_yields_the_unpublished_view() {
        let f = Fixture::new("unpublished");
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.pub_head, None);
        assert!(!view.related);
        assert_eq!(view.export_base, None);
        assert!(view.unreflected_pub.is_empty());
    }

    #[test]
    fn offline_without_a_tracking_ref_refuses_to_guess() {
        let f = Fixture::new("offline-nofetch");
        let err = load_sync_view(f.root(), &f.subrepo(), &SyncViewOptions { offline: true })
            .expect_err("no fetch yet");
        assert!(matches!(err, SyncViewError::NoFetchYet { .. }));
        assert_eq!(
            err.to_string(),
            "core: no fetch yet — run without --offline first"
        );
    }

    #[test]
    fn a_source_trailer_derives_the_map_the_base_and_the_last_export() {
        let f = Fixture::new("source-anchor");
        let mono = f.sh("git rev-parse HEAD");
        let tree = f.sh("git rev-parse HEAD:core");
        let pub_sha = f.push_pub(
            &tree,
            None,
            &format!("first commit\n\nMonosplice-Source: {mono}\n"),
        );

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.pub_head.as_deref(), Some(pub_sha.as_str()));
        assert_eq!(
            view.exported_mono_to_pub.get(&mono).map(String::as_str),
            Some(pub_sha.as_str())
        );
        assert_eq!(view.export_base.as_deref(), Some(mono.as_str()));
        assert_eq!(view.last_exported_mono.as_deref(), Some(mono.as_str()));
        assert!(view.related);
        // Our own export is not something to pull back in.
        assert!(view.unreflected_pub.is_empty());
        assert!(view.broken_source_refs.is_empty());

        // The tracking ref is now on disk, so the offline view agrees.
        let offline = load_sync_view(f.root(), &f.subrepo(), &SyncViewOptions { offline: true })
            .expect("offline view");
        assert_eq!(offline.pub_head, view.pub_head);
        assert_eq!(offline.export_base, view.export_base);
    }

    #[test]
    fn an_origin_trailer_anchors_only_when_the_tree_matches() {
        let f = Fixture::new("origin-anchor");
        // A public commit with content the monorepo will import.
        let blob = f.sh("printf 'from pub\n' | git hash-object -w --stdin");
        let pub_tree = f.sh(&format!(
            "printf '100644 blob {blob}\\tREADME.md\\n' | git mktree"
        ));
        let pub_sha = f.push_pub(&pub_tree, None, "public work\n");

        // A *clean* import: core/ ends up byte-identical to the pub tree.
        f.sh("printf 'from pub\n' > core/README.md");
        let mono = f.commit(&format!("import\n\nMonosplice-Origin: {pub_sha}\n"));
        assert_eq!(f.sh("git rev-parse HEAD:core"), pub_tree);

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(view.related);
        assert_eq!(view.export_base.as_deref(), Some(mono.as_str()));
        assert!(view.imported_pub_shas.contains(&pub_sha));
        // Reflected by ancestry, so nothing is pending.
        assert!(view.unreflected_pub.is_empty());
        // No Source trailer anywhere: nothing was ever exported.
        assert_eq!(view.last_exported_mono, None);

        // Now break the tree equality: a *conflicted* import carries work pub has never
        // seen, so it must not become the export boundary.
        f.sh("printf 'resolved differently\n' > core/README.md");
        let conflicted = f.commit(&format!(
            "conflicted import\n\nMonosplice-Origin: {pub_sha}\n"
        ));
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(view.related);
        // The walk skips the conflicted commit and lands on the clean one below it.
        assert_eq!(view.export_base.as_deref(), Some(mono.as_str()));
        assert_ne!(view.export_base.as_deref(), Some(conflicted.as_str()));
    }

    #[test]
    fn an_origin_trailer_that_never_matches_leaves_no_base_but_still_relates() {
        let f = Fixture::new("origin-no-match");
        let blob = f.sh("printf 'from pub\n' | git hash-object -w --stdin");
        let pub_tree = f.sh(&format!(
            "printf '100644 blob {blob}\\tREADME.md\\n' | git mktree"
        ));
        let pub_sha = f.push_pub(&pub_tree, None, "public work\n");

        f.sh("printf 'never matches\n' > core/README.md");
        f.commit(&format!("bad import\n\nMonosplice-Origin: {pub_sha}\n"));

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        // Named a real pub ancestor, so the repos know about each other...
        assert!(view.related);
        // ...but no commit reproduces pub, so there is no boundary to append after.
        assert_eq!(view.export_base, None);
    }

    #[test]
    fn a_pub_commit_naming_an_unknown_mono_sha_is_a_broken_source_ref() {
        let f = Fixture::new("broken-source");
        let tree = f.sh("git rev-parse HEAD:core");
        let bogus = "0".repeat(40);
        let pub_sha = f.push_pub(
            &tree,
            None,
            &format!("export\n\nMonosplice-Source: {bogus}\n"),
        );

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.broken_source_refs.len(), 1);
        assert_eq!(view.broken_source_refs[0].pub_sha, pub_sha);
        assert_eq!(view.broken_source_refs[0].mono_sha, bogus);
        // A broken claim never becomes the rewrite-detection cursor.
        assert_eq!(view.last_exported_mono, None);
        // ...and it still counts as "related": pub is talking about us.
        assert!(view.related);
        // Nothing resolves anywhere, so nothing supersedes it either.
        assert!(view.superseded_source_refs.is_empty());
    }

    /// Validation stops at the newest trailer that resolves. Below that point a dead sha is a
    /// fossil of somebody's rebase — no clone will ever have it, and it cannot change what is
    /// published. Above it, the mapping is unreadable and the refusal stands.
    #[test]
    fn a_dead_source_ref_is_superseded_below_the_live_anchor_and_broken_above_it() {
        let f = Fixture::new("superseded-source");
        let mono = f.sh("git rev-parse HEAD");
        let tree = f.sh("git rev-parse HEAD:core");
        let rebased_away = "0".repeat(40);
        let elsewhere = "1".repeat(40);

        let old = f.push_pub(
            &tree,
            None,
            &format!("export from before a rebase\n\nMonosplice-Source: {rebased_away}\n"),
        );
        let live = f.push_pub(
            &tree,
            Some(&old),
            &format!("export that healed it\n\nMonosplice-Source: {mono}\n"),
        );

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(
            view.broken_source_refs.is_empty(),
            "{:?}",
            view.broken_source_refs
        );
        assert_eq!(view.superseded_source_refs.len(), 1);
        assert_eq!(view.superseded_source_refs[0].pub_sha, old);
        assert_eq!(view.superseded_source_refs[0].mono_sha, rebased_away);
        assert_eq!(view.last_exported_mono.as_deref(), Some(mono.as_str()));
        assert_eq!(view.export_base.as_deref(), Some(mono.as_str()));

        // Now a public commit above the live anchor names a commit nobody here has: that
        // stretch of public history cannot be checked, and the refusal comes back.
        f.push_pub(
            &tree,
            Some(&live),
            &format!("published from somewhere else\n\nMonosplice-Source: {elsewhere}\n"),
        );
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.broken_source_refs.len(), 1);
        assert_eq!(view.broken_source_refs[0].mono_sha, elsewhere);
        assert_eq!(
            view.superseded_source_refs.len(),
            1,
            "still just the fossil"
        );
        assert_eq!(view.last_exported_mono.as_deref(), Some(mono.as_str()));
    }

    #[test]
    fn unreflected_pub_is_ancestry_based_not_per_commit() {
        let f = Fixture::new("unreflected");
        let blob1 = f.sh("printf 'one\n' | git hash-object -w --stdin");
        let t1 = f.sh(&format!(
            "printf '100644 blob {blob1}\\ta.txt\\n' | git mktree"
        ));
        let p1 = f.push_pub(&t1, None, "pub one\n");
        let blob2 = f.sh("printf 'two\n' | git hash-object -w --stdin");
        let t2 = f.sh(&format!(
            "printf '100644 blob {blob1}\\ta.txt\\n100644 blob {blob2}\\tb.txt\\n' | git mktree"
        ));
        let p2 = f.push_pub(&t2, Some(&p1), "pub two\n");
        let blob3 = f.sh("printf 'three\n' | git hash-object -w --stdin");
        let t3 = f.sh(&format!(
            "printf '100644 blob {blob1}\\ta.txt\\n100644 blob {blob2}\\tb.txt\\n100644 blob {blob3}\\tc.txt\\n' | git mktree"
        ));
        let p3 = f.push_pub(&t3, Some(&p2), "pub three\n");

        // Nothing imported yet: all three are pending, oldest first.
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(
            view.unreflected_pub,
            vec![p1.clone(), p2.clone(), p3.clone()]
        );

        // Import only the middle one. Its ancestor p1 is reflected by construction.
        f.commit(&format!("import\n\nMonosplice-Origin: {p2}\n"));
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.unreflected_pub, vec![p3.clone()]);
    }

    #[test]
    fn a_forged_origin_value_cannot_abort_the_pending_walk() {
        let f = Fixture::new("forged-origin");
        let blob = f.sh("printf 'one\n' | git hash-object -w --stdin");
        let t1 = f.sh(&format!(
            "printf '100644 blob {blob}\\ta.txt\\n' | git mktree"
        ));
        let p1 = f.push_pub(&t1, None, "pub one\n");
        f.commit(&format!(
            "import\n\nMonosplice-Origin: {}\n",
            "0".repeat(40)
        ));

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.unreflected_pub, vec![p1]);
    }

    #[test]
    fn our_own_exports_never_read_as_pending_imports() {
        let f = Fixture::new("own-exports");
        let mono = f.sh("git rev-parse HEAD");
        let tree = f.sh("git rev-parse HEAD:core");
        f.push_pub(
            &tree,
            None,
            &format!("first commit\n\nMonosplice-Source: {mono}\n"),
        );
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(view.unreflected_pub.is_empty());
    }

    // --- fork state ---

    #[test]
    fn fork_state_is_none_until_the_push_branch_exists_and_then_mirrors_it() {
        let f = Fixture::new("fork-state");
        let mut s = f.subrepo();
        s.upstream = Some("upstream-does-not-matter-here".to_string());
        s.push_branch = "patches".to_string();

        assert_eq!(load_fork_state(f.root(), &s, &online()).unwrap().head, None);

        let head = f.sh("git rev-parse HEAD");
        f.sh(&format!(
            "git push -q {} {head}:refs/heads/patches",
            f.remote_url()
        ));
        let state = load_fork_state(f.root(), &s, &online()).expect("fork state");
        assert_eq!(state.head.as_deref(), Some(head.as_str()));
        // The fetch really mirrored it into the fork tracking ref, so offline agrees.
        let offline = load_fork_state(f.root(), &s, &SyncViewOptions { offline: true }).unwrap();
        assert_eq!(offline.head.as_deref(), Some(head.as_str()));
    }

    #[test]
    fn an_unreachable_fork_is_a_note_not_a_crash() {
        let f = Fixture::new("fork-unreachable");
        let mut s = f.subrepo();
        s.remote = f.dir.join("nope.git").display().to_string();
        let (state, error) = try_load_fork_state(f.root(), &s, &online());
        assert!(state.is_none());
        assert!(error.is_some(), "expected a GitError");
        assert!(load_fork_state(f.root(), &s, &online()).is_err());
    }
}
