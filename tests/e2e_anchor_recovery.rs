//! e2e: a rebase that leaves the subrepo tree untouched must not strand `push`.
//!
//! The anchor is a *sha* recorded in a `Monosplice-Source` trailer, but what it stands for is a
//! *tree*: "the standalone repo already carries everything this commit publishes". Rebasing the
//! monorepo over unrelated commits rewrites the sha and keeps the tree, so the recorded anchor
//! disappears from HEAD's history while the export it names is still perfectly correct.
//!
//! Before the fix `push` refused with "restore that commit (`git reflog`)" — advice that is
//! wrong for this case, because nothing was lost. Now the anchor is re-derived from content:
//! the newest commit on the HEAD walk whose publishable subtree equals the tree the standalone
//! repo already has is adopted in its place. A rewrite that genuinely changed the subrepo tree
//! still refuses, and a recovered anchor never excuses unimported standalone commits.

mod common;

use std::path::Path;

use common::{clone_remote, run_monosplice, standard_fixture, Fixture, TestRepo};

const EXT_NAME: &str = "Ext Contributor";
const EXT_EMAIL: &str = "ext@example.test";

fn run_ok(dir: &Path, args: &[&str]) {
    let res = run_monosplice(dir, args);
    assert_eq!(
        res.exit_code,
        0,
        "`monosplice {}` failed: {}",
        args.join(" "),
        res.stderr
    );
}

struct Exported {
    fx: Fixture,
    pub_repo: TestRepo,
    /// Monorepo commit the baseline publish recorded.
    base: String,
    /// Monorepo commit the second push exported — the sha a rebase will rewrite away.
    exported: String,
    /// Standalone head after that push.
    pub_head: String,
}

/// A published subrepo with one exported commit on top of the baseline.
fn exported_once() -> Exported {
    let fx = standard_fixture();
    run_ok(&fx.mono.dir, &["push", "core", "--yes"]);
    let base = fx.mono.head();

    fx.mono.commit(
        "feat: exported before the rewrite",
        &[("core/one.ts", Some("export const one = 1\n"))],
    );
    run_ok(&fx.mono.dir, &["push"]);

    let pub_repo = TestRepo::new(&fx.pub_dir);
    let exported = fx.mono.head();
    let pub_head = pub_repo.head();
    Exported {
        fx,
        pub_repo,
        base,
        exported,
        pub_head,
    }
}

/// Rebase `main` over one commit that touches nothing under `core/` — the ordinary "rebase onto
/// updated main" that rewrites every sha above `base` and no subrepo tree at all.
fn rebase_over_unrelated(mono: &TestRepo, base: &str) {
    mono.git(&["checkout", "-q", "-b", "unrelated", base]);
    mono.commit(
        "chore: unrelated top-level work",
        &[("app/main.ts", Some("export const app = true\n"))],
    );
    mono.git(&["checkout", "-q", "main"]);
    mono.git(&["rebase", "-q", "unrelated"]);
}

#[test]
fn a_rebase_that_keeps_the_subrepo_tree_recovers_the_anchor_and_exports_only_new_work() {
    let ex = exported_once();
    let mono = &ex.fx.mono;
    let core_tree = mono.tree_sha("HEAD", Some("core"));

    rebase_over_unrelated(mono, &ex.base);
    let rewritten = mono.head();
    assert_ne!(rewritten, ex.exported, "the rebase must rewrite the sha");
    assert_eq!(
        mono.tree_sha("HEAD", Some("core")),
        core_tree,
        "...and must leave the subrepo tree byte-identical"
    );

    mono.commit(
        "feat: after the rebase",
        &[("core/two.ts", Some("export const two = 2\n"))],
    );

    // The preview agrees with the push: exactly one commit, no refusal.
    let dry = run_monosplice(&mono.dir, &["push", "--dry-run"]);
    assert_eq!(dry.exit_code, 0, "stderr: {}", dry.stderr);
    assert!(
        dry.stdout.contains("core: 1 to push"),
        "got:\n{}",
        dry.stdout
    );
    assert_eq!(
        ex.pub_repo.head(),
        ex.pub_head,
        "a dry run must not write to the remote"
    );

    let push = run_monosplice(&mono.dir, &["push"]);
    assert_eq!(push.exit_code, 0, "stderr: {}", push.stderr);
    assert!(
        push.stderr.contains(&format!(
            "recovered anchor: {} → {rewritten} (identical subrepo tree after history rewrite)",
            ex.exported
        )),
        "got:\n{}",
        push.stderr
    );
    assert!(
        push.stdout.contains("exported 1 commit"),
        "got:\n{}",
        push.stdout
    );

    // Exactly one new public commit, on top of the one the pre-rebase push created.
    assert_eq!(ex.pub_repo.git(&["rev-parse", "HEAD~1"]), ex.pub_head);
    assert_eq!(
        ex.pub_repo.subjects("HEAD").last().map(String::as_str),
        Some("feat: after the rebase")
    );
    assert_eq!(
        ex.pub_repo.tree_sha("HEAD", None),
        mono.tree_sha("HEAD", Some("core")),
        "pub tree == filtered(mono HEAD) still holds"
    );

    // Self-healing: the export just recorded a live sha, so the next run is ordinary.
    let again = run_monosplice(&mono.dir, &["push"]);
    assert_eq!(again.exit_code, 0, "stderr: {}", again.stderr);
    assert!(
        !again.stderr.contains("recovered anchor"),
        "got:\n{}",
        again.stderr
    );
    assert!(
        again.stdout.contains("up to date"),
        "got:\n{}",
        again.stdout
    );
}

#[test]
fn doctor_reports_the_anchor_recovery_it_would_do_instead_of_only_the_error() {
    let ex = exported_once();
    let mono = &ex.fx.mono;

    rebase_over_unrelated(mono, &ex.base);
    let rewritten = mono.head();

    let doc = run_monosplice(&mono.dir, &["doctor"]);
    assert!(
        doc.stdout
            .contains(&format!("recoverable via identical tree at {rewritten}")),
        "got:\n{}",
        doc.stdout
    );
    assert!(
        doc.stdout.contains(&ex.exported),
        "the stale sha is still named:\n{}",
        doc.stdout
    );
    assert_eq!(
        doc.exit_code, 0,
        "a recoverable anchor is a note, not a problem:\n{}",
        doc.stdout
    );
    assert!(
        !doc.stdout.contains("git reflog"),
        "nothing needs restoring:\n{}",
        doc.stdout
    );
}

#[test]
fn a_rewrite_that_changed_the_subrepo_tree_is_still_refused() {
    let ex = exported_once();
    let mono = &ex.fx.mono;

    // Same shape of rewrite, but the replacement publishes different content: nothing on the
    // HEAD walk reproduces what the standalone repo carries, so there is no anchor to recover.
    mono.git(&["reset", "--hard", &ex.base]);
    mono.commit(
        "feat: rewritten with different content",
        &[("core/one.ts", Some("export const one = 999\n"))],
    );

    let push = run_monosplice(&mono.dir, &["push"]);
    assert_ne!(push.exit_code, 0, "stdout: {}", push.stdout);
    assert!(
        push.stderr.contains(&ex.exported)
            && push.stderr.contains("is no longer an ancestor of HEAD"),
        "got:\n{}",
        push.stderr
    );
    assert!(
        !push.stderr.contains("recovered anchor"),
        "got:\n{}",
        push.stderr
    );
    assert_eq!(ex.pub_repo.head(), ex.pub_head, "nothing was pushed");

    let doc = run_monosplice(&mono.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 1, "stdout: {}", doc.stdout);
    assert!(
        !doc.stdout.contains("recoverable via identical tree"),
        "got:\n{}",
        doc.stdout
    );
    assert!(doc.stdout.contains("git reflog"), "got:\n{}", doc.stdout);
}

#[test]
fn a_recovered_anchor_does_not_excuse_unimported_standalone_commits() {
    let ex = exported_once();
    let mono = &ex.fx.mono;

    rebase_over_unrelated(mono, &ex.base);
    mono.commit(
        "feat: after the rebase",
        &[("core/two.ts", Some("export const two = 2\n"))],
    );

    // Meanwhile someone commits directly on the standalone repo.
    let ext = clone_remote(ex.fx.sandbox.path(), &ex.fx.pub_dir, "ext");
    ext.commit_as(
        "feat: from a contributor",
        &[("CONTRIB.md", Some("outside work\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    ext.git(&["push", "origin", "main"]);
    let pub_head = ex.pub_repo.head();

    let push = run_monosplice(&mono.dir, &["push"]);
    assert_ne!(push.exit_code, 0, "stdout: {}", push.stdout);
    assert!(
        push.stderr.contains("have not been imported yet"),
        "recovery re-derives the anchor and nothing else:\n{}",
        push.stderr
    );
    assert_eq!(ex.pub_repo.head(), pub_head, "nothing was pushed");
}
