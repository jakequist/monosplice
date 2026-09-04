//! Port of `src/core/exporter.ts` — replaying monorepo commits onto a public branch.
//!
//! Every path here is plumbing against the object db; the working tree and the index are
//! never touched (CLAUDE.md). A run resolves *every* commit — filters, scan hooks, rewritten
//! messages — before it writes to a remote, and then writes exactly once: a hook that rejects
//! must never leave a partially published branch behind.

use std::path::Path;

use crate::config::ResolvedSubrepo;
use crate::core::filter::{anchor_subtree, filtered_subtree, FilterError};
use crate::core::git::{
    commit_tree, git, git_ok, push_ref, push_ref_with_lease, read_commit, rev_list, CommitMeta,
    CommitTreeInput, GitError, EMPTY_TREE,
};
use crate::core::hooks::{run_rewrite_message, HookError};
use crate::core::sync_view::{
    fork_tracking_ref, load_fork_state, remote_tracking_ref, unpublished_view, SyncView,
    SyncViewOptions,
};
use crate::core::trailers::{append_trailer, get_trailer, ORIGIN_TRAILER, SOURCE_TRAILER};

#[derive(Debug, Clone)]
pub struct ExportCandidate {
    pub mono_sha: String,
}

/// A monorepo commit that really would become a public commit, fully resolved but uncommitted.
#[derive(Debug, Clone)]
pub struct PlannedExport {
    pub mono_sha: String,
    /// Filtered subtree sha (excludes and hooks already applied).
    pub tree: String,
    /// Final public commit message, trailer included.
    pub message: String,
    pub meta: CommitMeta,
}

#[derive(Debug, Clone)]
pub struct ExportedCommit {
    pub mono_sha: String,
    pub pub_sha: String,
}

#[derive(Debug, Clone)]
pub struct ExportResult {
    pub exported: Vec<ExportedCommit>,
    /// Public head after the run (unchanged when nothing was exported).
    pub new_head: Option<String>,
    /// Did this run write to a remote? False in triangular mode when the fork branch already
    /// carries exactly these commits — the export is built, byte-identical, and simply waiting
    /// for upstream to merge it.
    pub pushed: bool,
}

#[derive(Debug)]
pub enum ExportError {
    Hook(HookError),
    Git(GitError),
    Filter(FilterError),
    Other(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Hook(e) => write!(f, "{e}"),
            ExportError::Git(e) => write!(f, "{e}"),
            ExportError::Filter(e) => write!(f, "{e}"),
            ExportError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<HookError> for ExportError {
    fn from(e: HookError) -> Self {
        ExportError::Hook(e)
    }
}

impl From<GitError> for ExportError {
    fn from(e: GitError) -> Self {
        ExportError::Git(e)
    }
}

impl From<FilterError> for ExportError {
    fn from(e: FilterError) -> Self {
        ExportError::Filter(e)
    }
}

/// Monorepo commits eligible for export: everything touching the subrepo path since the
/// derived base, minus commits already exported.
///
/// Imported commits (`Monosplice-Origin`) are deliberately NOT filtered here. A pure import
/// reproduces the public tip's tree, so `run_export`'s tree-equality check drops it; a
/// *conflicted* import carries the user's merge resolution and must be exported, or
/// `pub tree == filtered(mono HEAD)` would stop holding.
pub fn plan_export(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
) -> Result<Vec<ExportCandidate>, ExportError> {
    let range = match &view.export_base {
        Some(base) => format!("{base}..HEAD"),
        None => "HEAD".to_string(),
    };
    let shas = rev_list(
        root,
        &["--reverse", "--topo-order", &range, "--", &subrepo.path],
    )?;
    Ok(shas
        .into_iter()
        .filter(|sha| !view.exported_mono_to_pub.contains_key(sha))
        .map(|mono_sha| ExportCandidate { mono_sha })
        .collect())
}

/// Is this commit a *pure* import — one whose publishable subtree is byte-identical to the
/// public commit it was replayed from, which the public branch therefore already contains?
///
/// Still not a trailer test: the trailer only says where to look, tree equality decides. A
/// conflicted import carries the user's resolution, differs from its origin, and must be
/// exported. Comparing against the origin rather than the current pub tip matters once the
/// tip has moved on — otherwise a long-settled import becomes a candidate again and
/// republishes an old state on top of newer public work.
fn already_published(root: &Path, message: &str, tree: &str, pub_head: &str) -> bool {
    let Some(origin) = get_trailer(message, ORIGIN_TRAILER) else {
        return false;
    };
    if origin.is_empty() {
        return false;
    }
    if !git_ok(root, &["merge-base", "--is-ancestor", &origin, pub_head]) {
        return false;
    }
    match git(root, &["rev-parse", &format!("{origin}^{{tree}}")]) {
        Ok(origin_tree) => origin_tree == tree,
        Err(_) => false,
    }
}

/// Resolve what the candidates would publish — filtered trees, hooks, rewritten messages,
/// tree-equality skips — without creating a single object or touching a remote. `run_export`
/// builds on this; `status`/`doctor` use it to answer "how many commits would push create?"
/// accurately, which a raw candidate count cannot do.
pub fn compute_exports(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
    candidates: &[ExportCandidate],
) -> Result<Vec<PlannedExport>, ExportError> {
    let mut tip_tree = match &view.pub_head {
        None => EMPTY_TREE.to_string(),
        Some(head) => git(root, &["rev-parse", &format!("{head}^{{tree}}")])?,
    };
    let mut planned: Vec<PlannedExport> = Vec::new();

    for candidate in candidates {
        let tree = filtered_subtree(root, &candidate.mono_sha, subrepo)?;
        // No subrepo content at this commit, or nothing publishable changed (e.g. only
        // excluded files) — an empty pub commit would be noise.
        let Some(tree) = tree else { continue };
        if tree == tip_tree {
            continue;
        }

        let meta = read_commit(root, &candidate.mono_sha)?;
        if let Some(pub_head) = &view.pub_head {
            if already_published(root, &meta.message, &tree, pub_head) {
                continue;
            }
        }

        let mut message = meta.message.clone();
        if let Some(cmd) = &subrepo.rewrite_message {
            message = run_rewrite_message(cmd, root, &subrepo.name, &meta.sha, &meta.message)?;
        }
        message = append_trailer(&message, SOURCE_TRAILER, &meta.sha);

        planned.push(PlannedExport {
            mono_sha: meta.sha.clone(),
            tree: tree.clone(),
            message,
            meta,
        });
        tip_tree = tree;
    }

    Ok(planned)
}

/// Turn planned exports into commit objects on top of `base`, without touching any remote.
///
/// Every input is fixed — tree, message, author *and* committer are copied from the monorepo
/// commit — so replaying the same plan on the same base always yields the same shas. That
/// determinism is what lets triangular mode recognise a fork branch it built earlier instead
/// of force-pushing an identical chain on every run.
pub fn build_export_chain(
    root: &Path,
    planned: &[PlannedExport],
    base: Option<&str>,
) -> Result<(Vec<ExportedCommit>, Option<String>), ExportError> {
    let mut tip: Option<String> = base.map(str::to_string);
    let mut exported: Vec<ExportedCommit> = Vec::new();
    for p in planned {
        let pub_sha = commit_tree(
            root,
            &CommitTreeInput {
                tree: p.tree.clone(),
                parents: tip.clone().into_iter().collect(),
                message: p.message.clone(),
                author_name: p.meta.author_name.clone(),
                author_email: p.meta.author_email.clone(),
                author_date: p.meta.author_date.clone(),
                committer_name: p.meta.committer_name.clone(),
                committer_email: p.meta.committer_email.clone(),
                committer_date: p.meta.committer_date.clone(),
            },
        )?;
        exported.push(ExportedCommit {
            mono_sha: p.mono_sha.clone(),
            pub_sha: pub_sha.clone(),
        });
        tip = Some(pub_sha);
    }
    Ok((exported, tip))
}

/// Replay candidates onto the public branch. Every commit (and therefore every scan hook)
/// is resolved first; the remote is written exactly once, at the end. A hook that rejects must
/// never leave a partially published branch behind.
///
/// In triangular mode the chain is parented on the UPSTREAM head and lands on the fork's
/// `push_branch`: a linear, PR-ready branch that monosplice owns and rebuilds. Upstream is
/// never written to, and the upstream tracking ref is never moved to something upstream does
/// not have.
pub fn run_export(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
    candidates: &[ExportCandidate],
) -> Result<ExportResult, ExportError> {
    let planned = compute_exports(root, subrepo, view, candidates)?;
    let (exported, tip) = build_export_chain(root, &planned, view.pub_head.as_deref())?;

    let (true, Some(tip)) = (!exported.is_empty(), tip) else {
        return Ok(ExportResult {
            exported: Vec::new(),
            new_head: view.pub_head.clone(),
            pushed: false,
        });
    };

    if subrepo.upstream.is_some() {
        let fork = load_fork_state(root, subrepo, &SyncViewOptions { offline: false })?;
        if fork.head.as_deref() == Some(tip.as_str()) {
            return Ok(ExportResult {
                exported,
                new_head: Some(tip),
                pushed: false,
            });
        }
        let dst = format!("refs/heads/{}", subrepo.push_branch);
        match &fork.head {
            None => push_ref(root, &subrepo.remote, &tip, &dst)?,
            Some(expect) => push_ref_with_lease(root, &subrepo.remote, &tip, &dst, expect)?,
        }
        git(
            root,
            &["update-ref", &fork_tracking_ref(&subrepo.name), &tip],
        )?;
        return Ok(ExportResult {
            exported,
            new_head: Some(tip),
            pushed: true,
        });
    }

    push_ref(
        root,
        &subrepo.remote,
        &tip,
        &format!("refs/heads/{}", subrepo.branch),
    )?;
    git(root, &["update-ref", &view.tracking_ref, &tip])?;
    Ok(ExportResult {
        exported,
        new_head: Some(tip),
        pushed: true,
    })
}

/// Has monorepo history been rewritten under the last exported commit? Export appends to pub
/// assuming everything after the scan base is new; if the commit pub says it last exported is
/// no longer reachable from HEAD, the monorepo was rebased underneath the mapping. This has to
/// consult `last_exported_mono` rather than `export_base`: a rewritten-away commit is exactly
/// the one the HEAD walk cannot see.
pub fn export_base_rewritten(root: &Path, view: &SyncView) -> bool {
    let Some(last) = &view.last_exported_mono else {
        return false;
    };
    !git_ok(root, &["merge-base", "--is-ancestor", last, "HEAD"])
}

/// How far back a content-anchored recovery looks. A rewrite that moved the anchor further
/// than this is not the "rebased over unrelated commits" case this recovers, and an unbounded
/// walk would turn one stale sha into a scan of the whole monorepo.
pub const ANCHOR_RECOVERY_LIMIT: usize = 1000;

/// A stale anchor that content proves is *only* stale.
///
/// The anchor is recorded as a sha, but what it stands for is a tree: "the standalone repo
/// already carries everything this commit publishes". A rebase over commits that touched other
/// paths rewrites the sha and keeps the tree, so the recorded commit leaves HEAD's history
/// while the export it names stays correct. Re-deriving the sha from the tree is not a
/// weakening of the rewrite check — it is the same check, asked about the thing that matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRecovery {
    /// The sha pub records, no longer reachable from HEAD.
    pub missing: String,
    /// The commit adopted in its place: the newest one on the HEAD walk publishing that tree.
    pub recovered: String,
    /// The public commit whose tree both of them export.
    pub pub_sha: String,
    /// How many further commits directly below `recovered` publish the same tree. Identical
    /// trees mean identical exports, so which one is chosen cannot change what gets pushed —
    /// but the report says that the newest was taken.
    pub also_matching: usize,
}

impl AnchorRecovery {
    /// The one line a `push` prints about it.
    pub fn message(&self, subrepo: &str) -> String {
        let mut out = format!(
            "{subrepo}: recovered anchor: {} → {} (identical subrepo tree after history rewrite)",
            self.missing, self.recovered
        );
        if self.also_matching > 0 {
            out.push_str(&format!(
                " — newest of {} adjacent commits publishing that tree",
                self.also_matching + 1
            ));
        }
        out
    }
}

/// Look for a commit on HEAD's history that publishes exactly what the missing anchor
/// published. Read-only: `doctor` calls this to report the recovery `push` would do.
///
/// Comparison goes through [`anchor_subtree`], the same tree mapping the exporter publishes
/// with (excludes, `transform`) minus the `scan` hook, which judges content rather than
/// shaping it. First-parent, newest first, bounded by [`ANCHOR_RECOVERY_LIMIT`]: the first
/// match is the newest, and stopping there is what keeps the range as tight as the original.
///
/// A commit whose subtree cannot be resolved (a `transform` that fails on old content) is not a
/// match rather than an error: recovery must never invent a new way for `push` to fail.
pub fn find_anchor_recovery(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
) -> Result<Option<AnchorRecovery>, ExportError> {
    if !export_base_rewritten(root, view) {
        return Ok(None);
    }
    let Some(missing) = view.last_exported_mono.clone() else {
        return Ok(None);
    };
    let Some(pub_sha) = view.exported_mono_to_pub.get(&missing).cloned() else {
        return Ok(None);
    };
    let target = git(root, &["rev-parse", &format!("{pub_sha}^{{tree}}")])?;

    let limit = ANCHOR_RECOVERY_LIMIT.to_string();
    let mut recovered: Option<String> = None;
    let mut also_matching = 0usize;
    for sha in rev_list(root, &["--first-parent", "-n", &limit, "HEAD"])? {
        let tree = anchor_subtree(root, &sha, subrepo).ok().flatten();
        let matches = tree.as_deref() == Some(target.as_str());
        match (&recovered, matches) {
            (None, true) => recovered = Some(sha),
            (Some(_), true) => also_matching += 1,
            (Some(_), false) => break,
            (None, false) => {}
        }
    }

    Ok(recovered.map(|recovered| AnchorRecovery {
        missing,
        recovered,
        pub_sha,
        also_matching,
    }))
}

/// The monorepo commit that currently proves the two repositories agree on a state: the newest
/// `Monosplice-Source` the public branch records that this clone has *and* that HEAD descends
/// from, or — when a rewrite moved it — the commit that publishes the same tree today.
///
/// It is the one fact that settles history below it. Everything up to a live anchor is
/// published by construction, whatever the trailers down there happen to say, which is what lets
/// `doctor` tell a fossil from a broken mapping. `None` means nothing vouches for anything:
/// never published, the wrong remote, or a mapping that cannot be read.
pub fn live_export_anchor(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
) -> Option<String> {
    let last = view.last_exported_mono.clone()?;
    if git_ok(root, &["merge-base", "--is-ancestor", &last, "HEAD"]) {
        return Some(last);
    }
    find_anchor_recovery(root, subrepo, view)
        .ok()
        .flatten()
        .map(|recovery| recovery.recovered)
}

/// Adopt a recovered anchor into the view, so the export range is derived from it. Returns
/// `None` when the anchor is intact or nothing on the HEAD walk reproduces the published tree —
/// in which case [`check_export_preconditions`] still refuses, exactly as before.
///
/// Only the anchor moves. Divergence (unimported public commits) is decided elsewhere, from
/// data this does not touch.
pub fn recover_export_anchor(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &mut SyncView,
) -> Result<Option<AnchorRecovery>, ExportError> {
    let Some(recovery) = find_anchor_recovery(root, subrepo, view)? else {
        return Ok(None);
    };
    // The scan base only ever moves forward: a surviving anchor newer than the recovered one
    // already proves pub carries everything up to it, and widening the range would replay
    // commits pub has.
    let keep_base = match &view.export_base {
        Some(base) => git_ok(
            root,
            &["merge-base", "--is-ancestor", &recovery.recovered, base],
        ),
        None => false,
    };
    if !keep_base {
        view.export_base = Some(recovery.recovered.clone());
    }
    view.last_exported_mono = Some(recovery.recovered.clone());
    view.exported_mono_to_pub
        .insert(recovery.recovered.clone(), recovery.pub_sha.clone());
    Ok(Some(recovery))
}

/// Why export must not run, or `None` when the derived mapping is trustworthy. Callers run
/// [`recover_export_anchor`] first: a stale anchor that content can re-derive is not a reason
/// to stop, and this refuses only what is left.
pub fn check_export_preconditions(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
) -> Option<String> {
    if let Some(broken) = view.broken_source_refs.first() {
        return Some(format!(
            "{}: standalone commit {} carries {SOURCE_TRAILER}: {}, but that monorepo commit does not exist in this clone.\nThe commit mapping is broken, so monosplice cannot tell what is already published and will not export on top of it. Nothing was pushed to {}.\nRun `monosplice doctor` to see the full picture.",
            subrepo.name, broken.pub_sha, broken.mono_sha, subrepo.remote,
        ));
    }

    if export_base_rewritten(root, view) {
        return Some(format!(
            "{}: the last exported monorepo commit {} is no longer an ancestor of HEAD.\nMonorepo history was rewritten (rebase, amend or force-push) underneath it, so monosplice cannot tell which commits are new. Nothing was pushed to {}.\nRun `monosplice doctor` for details, then restore that commit (`git reflog`) before pushing again.",
            subrepo.name,
            view.last_exported_mono.as_deref().unwrap_or(""),
            subrepo.remote,
        ));
    }

    None
}

/// The first public commit: the subrepo's current tree as one parentless commit. Returns
/// `None` when there is nothing publishable left after excludes and hooks. Object-db only,
/// like every other export path — the working tree is never touched.
pub fn publish_baseline(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    mono_head: &str,
) -> Result<Option<String>, ExportError> {
    let Some(tree) = filtered_subtree(root, mono_head, subrepo)? else {
        return Ok(None);
    };
    if tree == EMPTY_TREE {
        return Ok(None);
    }

    let meta = read_commit(root, mono_head)?;
    // A squashed baseline is not any one person's commit, so the committer identity stands in
    // for the author too: the monorepo author of HEAD did not write this snapshot.
    let pub_sha = commit_tree(
        root,
        &CommitTreeInput {
            tree,
            parents: Vec::new(),
            message: append_trailer(
                &format!("Initial import of {}\n", subrepo.name),
                SOURCE_TRAILER,
                &meta.sha,
            ),
            author_name: meta.committer_name.clone(),
            author_email: meta.committer_email.clone(),
            author_date: meta.committer_date.clone(),
            committer_name: meta.committer_name.clone(),
            committer_email: meta.committer_email.clone(),
            committer_date: meta.committer_date.clone(),
        },
    )?;

    push_ref(
        root,
        &subrepo.remote,
        &pub_sha,
        &format!("refs/heads/{}", subrepo.branch),
    )?;
    git(
        root,
        &["update-ref", &remote_tracking_ref(&subrepo.name), &pub_sha],
    )?;
    Ok(Some(pub_sha))
}

/// First publish that replays every monorepo commit touching the path instead of squashing.
/// Goes through `run_export`, so scan hooks run per replayed commit and a rejecting one aborts
/// before the single ref update — nothing partial ever reaches the remote.
pub fn publish_full_history(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    mono_head: &str,
) -> Result<ExportResult, ExportError> {
    let shas = rev_list(
        root,
        &["--reverse", "--topo-order", mono_head, "--", &subrepo.path],
    )?;
    let candidates: Vec<ExportCandidate> = shas
        .into_iter()
        .map(|mono_sha| ExportCandidate { mono_sha })
        .collect();
    run_export(root, subrepo, &unpublished_view(&subrepo.name), &candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sync_view::{load_sync_view, SyncViewOptions};
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
                "monosplice-exporter-{tag}-{}-{n}",
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
            ResolvedSubrepo {
                name: "core".to_string(),
                path: "core".to_string(),
                remote: self.remote_url(),
                upstream: None,
                branch: "main".to_string(),
                push_branch: "main".to_string(),
                exclude: Vec::new(),
                rewrite_message: None,
                transform: None,
                scan: None,
            }
        }

        fn next_date(&self) -> u64 {
            1_767_225_600 + (self.dates.fetch_add(1, Ordering::SeqCst) + 1) * 61
        }

        fn sh(&self, cmd: &str) -> String {
            sh(&self.mono, cmd, self.next_date())
        }

        fn remote_sh(&self, cmd: &str) -> String {
            sh(&self.remote, cmd, self.next_date())
        }

        fn commit(&self, message: &str) -> String {
            self.sh(&format!(
                "git add -A && git commit -q --allow-empty -m {}",
                shq(message)
            ));
            self.sh("git rev-parse HEAD")
        }

        fn view(&self, s: &ResolvedSubrepo) -> SyncView {
            load_sync_view(self.root(), s, &SyncViewOptions { offline: false }).expect("view")
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

    // --- plan / compute / run ---

    #[test]
    fn a_first_export_replays_one_commit_with_its_identity_and_trailer() {
        let f = Fixture::new("first-export");
        let s = f.subrepo();
        let mono = f.sh("git rev-parse HEAD");
        let view = f.view(&s);
        assert_eq!(view.pub_head, None);

        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].mono_sha, mono);

        let planned = compute_exports(f.root(), &s, &view, &candidates).expect("compute");
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].tree, f.sh("git rev-parse HEAD:core"));
        assert_eq!(
            planned[0].message,
            format!("first commit\n\nMonosplice-Source: {mono}\n")
        );

        let result = run_export(f.root(), &s, &view, &candidates).expect("export");
        assert!(result.pushed);
        assert_eq!(result.exported.len(), 1);
        let pub_sha = result.new_head.clone().expect("a new head");
        assert_eq!(result.exported[0].pub_sha, pub_sha);

        // The remote really moved, and the local tracking ref followed.
        assert_eq!(f.remote_sh("git rev-parse refs/heads/main"), pub_sha);
        assert_eq!(f.sh("git rev-parse refs/monosplice/core/remote"), pub_sha);
        // Author and committer are copied verbatim from the monorepo commit.
        assert_eq!(
            f.sh(&format!("git log -1 --format='%an|%ae|%cn|%ce' {pub_sha}")),
            "Mono Author|mono@example.test|Mono Author|mono@example.test"
        );
        assert_eq!(
            f.sh(&format!("git log -1 --format=%B {pub_sha}")),
            format!("first commit\n\nMonosplice-Source: {mono}")
        );
        // Only the subrepo content is published.
        assert_eq!(
            f.sh(&format!("git ls-tree --name-only {pub_sha}")),
            "README.md"
        );
    }

    #[test]
    fn a_commit_that_changes_nothing_publishable_is_skipped() {
        let f = Fixture::new("noop-skip");
        let mut s = f.subrepo();
        s.exclude = vec!["*.secret".to_string()];
        let view = f.view(&s);
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        run_export(f.root(), &s, &view, &candidates).expect("export");

        // A commit that only touches files outside the subrepo is not even a candidate...
        f.sh("printf 'more\n' > top.txt");
        f.commit("outside only");
        let view = f.view(&s);
        assert!(plan_export(f.root(), &s, &view).expect("plan").is_empty());

        // ...and a commit inside the path that only adds excluded content plans to nothing:
        // an empty public commit would be noise.
        f.sh("printf 'shh\n' > core/keys.secret");
        f.commit("add a secret");
        let view = f.view(&s);
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        assert_eq!(candidates.len(), 1, "it is a candidate...");
        assert!(
            compute_exports(f.root(), &s, &view, &candidates)
                .expect("compute")
                .is_empty(),
            "...but tree equality drops it"
        );
        let result = run_export(f.root(), &s, &view, &candidates).expect("export");
        assert!(!result.pushed);
        assert_eq!(result.new_head, view.pub_head);
    }

    /// The `alreadyPublished` rule, built by hand so the origin is a strict *ancestor* of the
    /// public tip: comparing against the tip instead would republish a long-settled import on
    /// top of newer public work.
    #[test]
    fn a_pure_import_is_dropped_but_a_conflicted_one_keeps_its_resolution() {
        let f = Fixture::new("already-published");
        let s = f.subrepo();

        // Public history, plumbing only: pub_a, then pub_b on top — the tip has moved on.
        let blob = f.sh("printf 'from pub\n' | git hash-object -w --stdin");
        let pub_tree = f.sh(&format!(
            "printf '100644 blob {blob}\\tREADME.md\\n' | git mktree"
        ));
        let pub_a = f.sh(&format!("printf 'pub a\n' | git commit-tree {pub_tree} "));
        let blob2 = f.sh("printf 'later\n' | git hash-object -w --stdin");
        let pub_tree2 = f.sh(&format!(
            "printf '100644 blob {blob}\\tREADME.md\\n100644 blob {blob2}\\tLATER.md\\n' | git mktree"
        ));
        let pub_b = f.sh(&format!(
            "printf 'pub b\n' | git commit-tree {pub_tree2} -p {pub_a}"
        ));

        // A clean import of pub_a: core/ reproduces its tree byte for byte.
        f.sh("printf 'from pub\n' > core/README.md");
        let clean = f.commit(&format!("import a\n\nMonosplice-Origin: {pub_a}\n"));
        assert_eq!(f.sh("git rev-parse HEAD:core"), pub_tree);
        // A conflicted import of the same pub commit, carrying a resolution pub never saw.
        f.sh("printf 'merged by hand\n' > core/README.md");
        let conflicted = f.commit(&format!("resolve\n\nMonosplice-Origin: {pub_a}\n"));

        let view = SyncView {
            pub_head: Some(pub_b),
            ..unpublished_view(&s.name)
        };

        // Both are candidates: export never skips by trailer.
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        let shas: Vec<&str> = candidates.iter().map(|c| c.mono_sha.as_str()).collect();
        assert!(shas.contains(&clean.as_str()), "{shas:?}");
        assert!(shas.contains(&conflicted.as_str()), "{shas:?}");

        // The clean one reproduces its origin's tree, which pub already contains.
        let planned = compute_exports(
            f.root(),
            &s,
            &view,
            &[ExportCandidate {
                mono_sha: clean.clone(),
            }],
        )
        .expect("compute");
        assert!(planned.is_empty(), "a pure import is a no-op: {planned:?}");

        // The conflicted one does not, and must export or the resolution would be lost.
        let planned = compute_exports(
            f.root(),
            &s,
            &view,
            &[ExportCandidate {
                mono_sha: conflicted.clone(),
            }],
        )
        .expect("compute");
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].mono_sha, conflicted);
    }

    #[test]
    fn the_rewrite_message_hook_runs_before_the_trailer_is_appended() {
        let f = Fixture::new("rewrite-message");
        let mut s = f.subrepo();
        s.rewrite_message = Some("sed 's/^/[core] /'".to_string());
        let mono = f.sh("git rev-parse HEAD");
        let view = f.view(&s);
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        let planned = compute_exports(f.root(), &s, &view, &candidates).expect("compute");
        assert_eq!(
            planned[0].message,
            format!("[core] first commit\n\nMonosplice-Source: {mono}\n")
        );
    }

    #[test]
    fn a_rejecting_scan_hook_aborts_before_anything_is_pushed() {
        let f = Fixture::new("scan-abort");
        let mut s = f.subrepo();
        s.scan = Some("echo 'secret!' 1>&2; exit 1".to_string());
        let view = f.view(&s);
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        let err = run_export(f.root(), &s, &view, &candidates).expect_err("scan rejects");
        assert!(
            err.to_string()
                .starts_with("scan hook rejected core commit"),
            "{err}"
        );
        assert!(err.to_string().ends_with(": secret!"), "{err}");
        // Nothing reached the remote.
        assert_eq!(f.remote_sh("git for-each-ref --format='%(refname)'"), "");
    }

    #[test]
    fn build_export_chain_is_deterministic_and_linear() {
        let f = Fixture::new("chain");
        let s = f.subrepo();
        f.sh("printf 'two\n' > core/b.txt");
        f.commit("second");
        let view = f.view(&s);
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        let planned = compute_exports(f.root(), &s, &view, &candidates).expect("compute");
        assert_eq!(planned.len(), 2);

        let (first, tip1) = build_export_chain(f.root(), &planned, None).expect("chain");
        let (second, tip2) = build_export_chain(f.root(), &planned, None).expect("chain again");
        assert_eq!(tip1, tip2, "same plan, same base, same shas");
        assert_eq!(
            first.iter().map(|e| e.pub_sha.clone()).collect::<Vec<_>>(),
            second.iter().map(|e| e.pub_sha.clone()).collect::<Vec<_>>()
        );
        let tip = tip1.expect("a tip");
        assert_eq!(
            f.sh(&format!("git rev-list --count {tip}")),
            "2",
            "one parentless root plus one child"
        );
        assert_eq!(f.sh(&format!("git rev-parse {tip}^")), first[0].pub_sha);
    }

    // --- rewrite detection & preconditions ---

    #[test]
    fn a_broken_source_ref_refuses_the_export_with_the_ts_wording() {
        let f = Fixture::new("broken-precondition");
        let s = f.subrepo();
        let tree = f.sh("git rev-parse HEAD:core");
        let bogus = "0".repeat(40);
        let pub_sha = f.sh(&format!(
            "printf 'export\n\nMonosplice-Source: {bogus}\n' | git commit-tree {tree} "
        ));
        f.sh(&format!(
            "git push -q {} {pub_sha}:refs/heads/main",
            f.remote_url()
        ));

        let view = f.view(&s);
        let message = check_export_preconditions(f.root(), &s, &view).expect("a refusal");
        assert_eq!(
            message,
            format!(
                "core: standalone commit {pub_sha} carries Monosplice-Source: {bogus}, but that monorepo commit does not exist in this clone.\nThe commit mapping is broken, so monosplice cannot tell what is already published and will not export on top of it. Nothing was pushed to {}.\nRun `monosplice doctor` to see the full picture.",
                f.remote_url()
            )
        );
    }

    #[test]
    fn rewritten_history_under_the_last_export_is_detected_and_refused() {
        let f = Fixture::new("rewritten");
        let s = f.subrepo();
        // Export a second commit, then throw it away with a reset.
        f.sh("printf 'two\n' > core/b.txt");
        f.commit("second");
        let view = f.view(&s);
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        run_export(f.root(), &s, &view, &candidates).expect("export");

        let view = f.view(&s);
        assert!(!export_base_rewritten(f.root(), &view));
        assert_eq!(check_export_preconditions(f.root(), &s, &view), None);
        assert_eq!(
            find_anchor_recovery(f.root(), &s, &view).expect("probe"),
            None,
            "an intact anchor is never 'recovered'"
        );

        let last = view.last_exported_mono.clone().expect("a last export");
        f.sh("git reset -q --hard HEAD~1");
        let view = f.view(&s);
        assert_eq!(view.last_exported_mono.as_deref(), Some(last.as_str()));
        assert!(export_base_rewritten(f.root(), &view));
        assert_eq!(
            find_anchor_recovery(f.root(), &s, &view).expect("probe"),
            None,
            "nothing on the HEAD walk publishes the exported tree any more"
        );
        let message = check_export_preconditions(f.root(), &s, &view).expect("a refusal");
        assert_eq!(
            message,
            format!(
                "core: the last exported monorepo commit {last} is no longer an ancestor of HEAD.\nMonorepo history was rewritten (rebase, amend or force-push) underneath it, so monosplice cannot tell which commits are new. Nothing was pushed to {}.\nRun `monosplice doctor` for details, then restore that commit (`git reflog`) before pushing again.",
                f.remote_url()
            )
        );
    }

    /// The production case: history was rewritten *above* the anchor without touching what the
    /// anchor publishes, so the sha is stale and the export it named is still correct.
    #[test]
    fn a_rewrite_that_keeps_the_subrepo_tree_recovers_the_anchor_and_prefers_the_newest() {
        let f = Fixture::new("anchor-recovery");
        let s = f.subrepo();
        let root_commit = f.sh("git rev-parse HEAD");

        // The ordinary first export of the root commit.
        let view = f.view(&s);
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        run_export(f.root(), &s, &view, &candidates).expect("export");
        let pub_head = f.sh("git rev-parse refs/monosplice/core/remote");

        // A second export, from a commit that changes nothing under core/ and then leaves the
        // main line — the rebased-away sha. Pub still records it.
        f.sh("git checkout -q -b rewritten-away");
        f.sh("printf 'moved on\n' > top.txt");
        let away = f.commit("work outside core");
        let tree = f.sh("git rev-parse HEAD:core");
        let p1 = f.sh(&format!(
            "printf 'export\n\nMonosplice-Source: {away}\n' | git commit-tree {tree} -p {pub_head}"
        ));
        f.sh(&format!(
            "git push -q {} {p1}:refs/heads/main",
            f.remote_url()
        ));

        // Back on main: two more commits, each publishing that same core tree.
        f.sh("git checkout -q main");
        let b = f.commit("nothing to do with core");
        let c = f.commit("also nothing to do with core");

        let mut view = f.view(&s);
        assert_eq!(view.last_exported_mono.as_deref(), Some(away.as_str()));
        assert!(export_base_rewritten(f.root(), &view));

        let recovery = find_anchor_recovery(f.root(), &s, &view)
            .expect("probe")
            .expect("the tree is still on the walk");
        assert_eq!(recovery.missing, away);
        assert_eq!(recovery.recovered, c, "the newest match wins");
        assert_eq!(recovery.pub_sha, p1);
        assert_eq!(
            recovery.also_matching, 2,
            "{b} and {root_commit} publish it too"
        );
        let line = recovery.message("core");
        assert_eq!(
            line,
            format!("core: recovered anchor: {away} → {c} (identical subrepo tree after history rewrite) — newest of 3 adjacent commits publishing that tree")
        );

        // Adopting it clears the refusal and tightens the range to "after the anchor".
        let applied = recover_export_anchor(f.root(), &s, &mut view)
            .expect("recover")
            .expect("recovered");
        assert_eq!(applied, recovery);
        assert_eq!(view.export_base.as_deref(), Some(c.as_str()));
        assert_eq!(view.last_exported_mono.as_deref(), Some(c.as_str()));
        assert!(!export_base_rewritten(f.root(), &view));
        assert_eq!(check_export_preconditions(f.root(), &s, &view), None);
        assert!(
            plan_export(f.root(), &s, &view).expect("plan").is_empty(),
            "everything up to the recovered anchor is already published"
        );
    }

    #[test]
    fn one_matching_commit_needs_no_word_about_which_one_was_chosen() {
        let recovery = AnchorRecovery {
            missing: "aaa".to_string(),
            recovered: "bbb".to_string(),
            pub_sha: "ccc".to_string(),
            also_matching: 0,
        };
        assert_eq!(
            recovery.message("core"),
            "core: recovered anchor: aaa → bbb (identical subrepo tree after history rewrite)"
        );
    }

    // --- first publish ---

    #[test]
    fn publish_baseline_pushes_one_parentless_snapshot() {
        let f = Fixture::new("baseline");
        let s = f.subrepo();
        f.sh("printf 'two\n' > core/b.txt");
        let mono = f.commit("second");

        let pub_sha = publish_baseline(f.root(), &s, "HEAD")
            .expect("baseline")
            .expect("something publishable");
        assert_eq!(f.remote_sh("git rev-parse refs/heads/main"), pub_sha);
        assert_eq!(f.sh("git rev-parse refs/monosplice/core/remote"), pub_sha);
        assert_eq!(f.sh(&format!("git rev-list --count {pub_sha}")), "1");
        assert_eq!(
            f.sh(&format!("git log -1 --format=%B {pub_sha}")),
            format!("Initial import of core\n\nMonosplice-Source: {mono}")
        );
        assert_eq!(
            f.sh(&format!("git rev-parse {pub_sha}^{{tree}}")),
            f.sh("git rev-parse HEAD:core")
        );
        // Author is the committer of the monorepo head — a snapshot is nobody's authorship.
        let (an, ad, cn, cd) = {
            let out = f.sh(&format!(
                "git log -1 --format='%an%x00%ad%x00%cn%x00%cd' --date=raw {pub_sha}"
            ));
            let mut it = out.split('\u{0}');
            (
                it.next().unwrap().to_string(),
                it.next().unwrap().to_string(),
                it.next().unwrap().to_string(),
                it.next().unwrap().to_string(),
            )
        };
        assert_eq!(an, cn);
        assert_eq!(ad, cd);
    }

    #[test]
    fn publish_baseline_is_none_when_everything_is_excluded() {
        let f = Fixture::new("baseline-empty");
        let mut s = f.subrepo();
        s.exclude = vec!["**/*".to_string()];
        assert_eq!(
            publish_baseline(f.root(), &s, "HEAD").expect("baseline"),
            None
        );
        assert_eq!(f.remote_sh("git for-each-ref --format='%(refname)'"), "");

        let mut missing = f.subrepo();
        missing.path = "nowhere".to_string();
        assert_eq!(
            publish_baseline(f.root(), &missing, "HEAD").expect("baseline"),
            None
        );
    }

    #[test]
    fn publish_full_history_replays_every_commit_touching_the_path() {
        let f = Fixture::new("full-history");
        let s = f.subrepo();
        f.sh("printf 'two\n' > core/b.txt");
        f.commit("second");
        f.sh("printf 'more\n' > top.txt");
        f.commit("outside the subrepo");
        f.sh("printf 'three\n' > core/c.txt");
        f.commit("third");

        let result = publish_full_history(f.root(), &s, "HEAD").expect("publish");
        assert!(result.pushed);
        assert_eq!(result.exported.len(), 3);
        let tip = result.new_head.clone().expect("a head");
        assert_eq!(f.remote_sh("git rev-parse refs/heads/main"), tip);
        assert_eq!(f.sh(&format!("git rev-list --count {tip}")), "3");
        assert_eq!(
            f.sh(&format!("git log --reverse --format=%s {tip}")),
            "first commit\nsecond\nthird"
        );
    }

    // --- triangular ---

    #[test]
    fn triangular_export_lands_on_the_fork_push_branch_and_reuses_an_identical_chain() {
        let f = Fixture::new("triangular");
        let fork = f.dir.join("fork.git");
        sh(&f.dir, &format!("git init -q --bare {}", fork.display()), 0);
        let mut s = f.subrepo();
        s.upstream = Some(f.remote_url());
        s.remote = fork.display().to_string();
        s.push_branch = "patches".to_string();

        // Upstream publishes a baseline the monorepo already reflects.
        let tree = f.sh("git rev-parse HEAD:core");
        let mono = f.sh("git rev-parse HEAD");
        let upstream_head = f.sh(&format!(
            "printf 'base\n\nMonosplice-Source: {mono}\n' | git commit-tree {tree} "
        ));
        f.sh(&format!(
            "git push -q {} {upstream_head}:refs/heads/main",
            f.remote_url()
        ));

        f.sh("printf 'patch\n' > core/p.txt");
        f.commit("a patch for upstream");

        let view = f.view(&s);
        assert_eq!(view.pub_head.as_deref(), Some(upstream_head.as_str()));
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        let result = run_export(f.root(), &s, &view, &candidates).expect("export");
        assert!(result.pushed);
        let tip = result.new_head.clone().expect("a tip");

        // The fork carries the patch branch; upstream was never written to.
        assert_eq!(sh(&fork, "git rev-parse refs/heads/patches", 0), tip);
        assert_eq!(f.remote_sh("git rev-parse refs/heads/main"), upstream_head);
        assert_eq!(f.sh("git rev-parse refs/monosplice/core/fork"), tip);
        // Parented on upstream's head, not on the fork.
        assert_eq!(f.sh(&format!("git rev-parse {tip}^")), upstream_head);

        // Running again rebuilds the identical chain, so nothing is pushed a second time.
        let view = f.view(&s);
        let candidates = plan_export(f.root(), &s, &view).expect("plan");
        let again = run_export(f.root(), &s, &view, &candidates).expect("export");
        assert!(!again.pushed, "the fork already carries this chain");
        assert_eq!(again.new_head.as_deref(), Some(tip.as_str()));
        assert_eq!(again.exported.len(), 1);
    }

    #[test]
    fn export_error_display_forwards_to_the_inner_error() {
        let hook = ExportError::Hook(HookError {
            hook: "scan",
            mono_sha: "abc".to_string(),
            subrepo: "core".to_string(),
            detail: "nope".to_string(),
        });
        assert_eq!(hook.to_string(), "scan hook rejected core commit abc: nope");
        assert_eq!(ExportError::Other("boom".to_string()).to_string(), "boom");
    }
}
