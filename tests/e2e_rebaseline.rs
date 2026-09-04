//! e2e: re-baselining a subrepo onto a brand-new public remote.
//!
//! The import-side mirror of the superseded-anchor rule. When a subrepo is re-published to a
//! freshly created repository, every clone's monorepo history still carries `Monosplice-Origin`
//! trailers from imports off the *previous* remote — shas the new remote has never heard of and
//! never will. They are provenance for a state the current remote and the monorepo have since
//! agreed on, not a broken mapping.
//!
//! The rule is symmetric with the export side: a live export anchor (the newest
//! `Monosplice-Source` that resolves against the current remote and reconciles with HEAD) proves
//! the two repositories agree at that point, so unresolvable import trailers *below* it are
//! settled by construction — informational, never fatal. Above it, or with no live anchor at all
//! (the genuinely wrong remote), the refusal is exactly what it always was.

mod common;

use std::path::Path;

use common::{
    clone_remote, make_bare_remote, make_repo, run_monosplice, standard_fixture, subrepo_block,
    toml_str, write_config, Fixture, TestRepo,
};

const EXT_NAME: &str = "Ext Contributor";
const EXT_EMAIL: &str = "ext@example.test";
const DEAD_SHA: &str = "0000000000000000000000000000000000000000";

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

fn point_config_at(mono: &TestRepo, remote: &str) {
    write_config(
        mono,
        &[&subrepo_block(&[
            ("name", &toml_str("core")),
            ("path", &toml_str("core")),
            ("remote", &toml_str(remote)),
        ])],
    );
}

struct Rebaselined {
    fx: Fixture,
    /// The new remote the config points at, published from scratch.
    new_pub: TestRepo,
    new_pub_dir: String,
    /// A commit on the *old* remote that monorepo history still claims to have imported.
    fossil: String,
    /// The monorepo commit the new baseline recorded — the live export anchor.
    live_anchor: String,
}

/// Publish to one remote, import a commit from it (leaving a `Monosplice-Origin` trailer), then
/// re-baseline: point the config at a brand-new empty repository and first-publish into it.
fn rebaselined_onto_a_new_remote() -> Rebaselined {
    let fx = standard_fixture();
    let mono = &fx.mono;
    run_ok(&mono.dir, &["push", "core", "--yes"]);

    // Someone commits on the old remote; the monorepo imports it, recording the provenance.
    let ext = clone_remote(fx.sandbox.path(), &fx.pub_dir, "ext");
    ext.commit_as(
        "feat: work on the old remote",
        &[("OLD.md", Some("published elsewhere\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    ext.git(&["push", "origin", "main"]);
    let fossil = ext.head();
    run_ok(&mono.dir, &["pull"]);
    assert!(
        mono.messages("HEAD")
            .iter()
            .any(|m| m.contains(&format!("Monosplice-Origin: {fossil}"))),
        "the import must record where it came from"
    );

    // Re-baseline: a brand-new empty repository, published from this clone.
    let new_pub_dir = make_bare_remote(fx.sandbox.path(), "core-pub-v2");
    point_config_at(mono, &new_pub_dir);
    mono.commit("chore: re-baseline core onto a new remote", &[]);
    run_ok(&mono.dir, &["push", "core", "--yes"]);
    let live_anchor = mono.head();

    let new_pub = TestRepo::new(&new_pub_dir);
    assert_eq!(
        new_pub.subjects("HEAD"),
        vec!["Initial import of core"],
        "the new remote starts from one baseline commit"
    );
    Rebaselined {
        fx,
        new_pub,
        new_pub_dir,
        fossil,
        live_anchor,
    }
}

#[test]
fn a_clone_with_import_trailers_from_the_old_remote_is_healthy_after_a_re_baseline() {
    let rb = rebaselined_onto_a_new_remote();

    // `file://` forces the pack protocol, so the clone gets only reachable objects — no
    // monosplice tracking refs, and nothing from the old remote.
    let url = format!("file://{}", rb.fx.mono.dir.display());
    let fresh = clone_remote(rb.fx.sandbox.path(), &url, "fresh");
    assert_ne!(
        fresh.git_try(&["cat-file", "-e", &rb.fossil]).exit_code,
        0,
        "the fresh clone has never seen the old remote's commit"
    );
    assert!(
        fresh
            .messages("HEAD")
            .iter()
            .any(|m| m.contains(&format!("Monosplice-Origin: {}", rb.fossil))),
        "...but its history still claims to have imported it"
    );

    let doc = run_monosplice(&fresh.dir, &["doctor"]);
    assert_eq!(
        doc.exit_code, 0,
        "settled provenance is not a problem:\n{}",
        doc.stdout
    );
    assert!(
        doc.stdout.contains("historical import trailer")
            && doc
                .stdout
                .contains(&format!("superseded by live anchor at {}", rb.live_anchor)),
        "got:\n{}",
        doc.stdout
    );
    assert!(
        doc.stdout.contains(&rb.fossil),
        "the fossil sha is still named:\n{}",
        doc.stdout
    );

    let dry = run_monosplice(&fresh.dir, &["push", "--dry-run"]);
    assert_eq!(dry.exit_code, 0, "stderr: {}", dry.stderr);
    assert!(
        dry.stdout.contains("core: up to date"),
        "got:\n{}",
        dry.stdout
    );

    // Real work from that clone reaches the new remote.
    let pub_head = rb.new_pub.head();
    fresh.commit(
        "feat: after the re-baseline",
        &[("core/next.ts", Some("export const next = true\n"))],
    );
    let push = run_monosplice(&fresh.dir, &["push"]);
    assert_eq!(push.exit_code, 0, "stderr: {}", push.stderr);
    assert!(
        push.stdout.contains("exported 1 commit"),
        "got:\n{}",
        push.stdout
    );
    assert_eq!(rb.new_pub.git(&["rev-parse", "HEAD~1"]), pub_head);
    assert_eq!(
        rb.new_pub.tree_sha("HEAD", None),
        fresh.tree_sha("HEAD", Some("core"))
    );
}

#[test]
fn an_import_trailer_above_the_live_anchor_is_still_a_problem() {
    let rb = rebaselined_onto_a_new_remote();
    let mono = &rb.fx.mono;

    // A commit *above* the live anchor claiming an import nothing can resolve: this one is not
    // settled by anything, so the mapping really is in question.
    mono.commit(
        &format!("chore: claims an import\n\nMonosplice-Origin: {DEAD_SHA}\n"),
        &[],
    );

    let doc = run_monosplice(&mono.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 1, "stdout: {}", doc.stdout);
    assert!(
        doc.stdout.contains(&format!(
            "✗ monorepo history claims to have imported commit {DEAD_SHA}"
        )),
        "got:\n{}",
        doc.stdout
    );
    // The settled one below the anchor is still only a note.
    assert!(
        doc.stdout.contains("historical import trailer"),
        "got:\n{}",
        doc.stdout
    );
}

#[test]
fn a_config_pointed_at_an_unrelated_remote_still_refuses() {
    let rb = rebaselined_onto_a_new_remote();
    let mono = &rb.fx.mono;

    // A repository with history of its own that was never published from here: no anchor of
    // either kind resolves, so nothing vouches for the import trailers.
    let stranger_dir = make_bare_remote(rb.fx.sandbox.path(), "stranger");
    let stranger = make_repo(rb.fx.sandbox.path(), "stranger-work");
    stranger.commit(
        "chore: someone else's repo",
        &[("README.md", Some("nope\n"))],
    );
    stranger.git(&["remote", "add", "origin", &stranger_dir]);
    stranger.git(&["push", "origin", "main"]);

    point_config_at(mono, &stranger_dir);
    mono.commit("chore: point core at an unrelated remote", &[]);

    let doc = run_monosplice(&mono.dir, &["doctor"]);
    assert_eq!(doc.exit_code, 1, "stdout: {}", doc.stdout);
    assert!(
        doc.stdout.contains(&format!(
            "✗ monorepo history claims to have imported commit {}",
            rb.fossil
        )),
        "got:\n{}",
        doc.stdout
    );
    assert!(
        !doc.stdout.contains("historical import trailer"),
        "nothing supersedes anything here:\n{}",
        doc.stdout
    );

    // ...and the previous remote is untouched by any of this.
    assert!(!rb.new_pub_dir.is_empty());
}
