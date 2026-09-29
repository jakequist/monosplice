//! e2e (S170–S175): chained monorepos, and trailers that belong to some other repository.
//!
//! An **outer** monorepo vendors a **middle** monorepo, which splices `lib/` out to a **leaf**
//! repo; or a monorepo vendors a repo that another monorepo publishes. Each hop records its
//! mapping in trailers, and before this fix a trailer written at one hop was copied into the
//! next repository, where it read as that repository's own claim; and a `Monosplice-Source`
//! naming another monorepo's commit was taken for this monorepo's export.
//!
//! The rules under test: a replay strips the sync trailers it finds and appends only its own
//! (S170); of the trailers already published, only the last one on a commit is its claim
//! (S171); a claim is this monorepo's only if it can be (S172, S173), and one that could still
//! be ours stops exactly as before (S174); `doctor` mentions the rest as information (S175).

mod common;

use std::path::Path;

use common::{
    clone_remote, make_bare_remote, make_repo, run_monosplice, sandbox, subrepo_block, toml_str,
    write_config, RunResult, Sandbox, TestRepo,
};

const SOURCE: &str = "Monosplice-Source";
const ORIGIN: &str = "Monosplice-Origin";

fn run_ok(dir: &Path, args: &[&str]) -> RunResult {
    let res = run_monosplice(dir, args);
    assert_eq!(
        res.exit_code,
        0,
        "`monosplice {}` failed in {}:\n{}\n{}",
        args.join(" "),
        dir.display(),
        res.stdout,
        res.stderr
    );
    res
}

/// The values of one trailer key on one commit, in order.
fn trailer(repo: &TestRepo, rev: &str, key: &str) -> Vec<String> {
    let out = repo.git(&[
        "log",
        "-1",
        &format!("--format=%(trailers:key={key},valueonly,separator=%x00)"),
        rev,
    ]);
    out.split('\0')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

fn config_entry(name: &str, path: &str, remote: &str) -> String {
    subrepo_block(&[
        ("name", &toml_str(name)),
        ("path", &toml_str(path)),
        ("remote", &toml_str(remote)),
    ])
}

/// leaf.git spliced out of middle's `lib/`, middle's working clone `mw` pushed to middle.git,
/// and `outer` vendoring middle.git at `vendor/middle` with a snapshot attach.
struct Chain {
    sb: Sandbox,
    leaf_dir: String,
    middle_dir: String,
    leaf: TestRepo,
    middle: TestRepo,
    mw: TestRepo,
    outer: TestRepo,
}

fn chain() -> Chain {
    let sb = sandbox();
    let root = sb.path();
    let leaf_dir = make_bare_remote(root, "leaf");
    let middle_dir = make_bare_remote(root, "middle");

    let mw = make_repo(root, "mw");
    write_config(&mw, &[&config_entry("lib", "lib", &leaf_dir)]);
    mw.commit(
        "middle: initial",
        &[
            ("lib/a.txt", Some("lib v1\n")),
            ("app/main.txt", Some("app\n")),
        ],
    );
    run_ok(&mw.dir, &["attach", "lib", "--yes"]);
    mw.git(&["remote", "add", "origin", &middle_dir]);
    mw.git(&["push", "-q", "origin", "main"]);

    let outer = make_repo(root, "outer");
    write_config(
        &outer,
        &[&config_entry("middle", "vendor/middle", &middle_dir)],
    );
    outer.commit("outer: initial", &[("README", Some("outer\n"))]);
    run_ok(&outer.dir, &["attach", "vendor/middle"]);

    Chain {
        leaf: TestRepo::new(&leaf_dir),
        middle: TestRepo::new(&middle_dir),
        sb,
        leaf_dir,
        middle_dir,
        mw,
        outer,
    }
}

impl Chain {
    /// A commit made directly on the leaf repository, pulled into middle and pushed to middle.git.
    fn leaf_commit_reaches_middle(&self) -> String {
        let lw = clone_remote(self.sb.path(), &self.leaf_dir, "lw");
        let leaf_sha = lw.commit("leaf: add b", &[("b.txt", Some("from leaf\n"))]);
        lw.git(&["push", "-q", "origin", "main"]);
        run_ok(&self.mw.dir, &["pull", "lib"]);
        self.mw.git(&["push", "-q", "origin", "main"]);
        leaf_sha
    }
}

// ---------------------------------------------------------------------------------------------
// S170: a replay never forwards sync trailers.
// ---------------------------------------------------------------------------------------------

/// outer -> middle -> leaf. Middle's export of a commit that arrived from outer carries only
/// middle's own `Monosplice-Source`: outer's (private) sha never reaches the leaf, other
/// trailers survive, and middle keeps pushing.
#[test]
fn s170_an_export_of_a_commit_from_an_outer_monorepo_carries_only_its_own_source() {
    let c = chain();
    c.outer.commit(
        "outer: patch lib\n\nSigned-off-by: Outer Dev <outer@example.test>",
        &[("vendor/middle/lib/a.txt", Some("lib v1\nouter patch\n"))],
    );
    let outer_sha = c.outer.head();
    run_ok(&c.outer.dir, &["push", "middle"]);
    assert_eq!(trailer(&c.middle, "main", SOURCE), vec![outer_sha.clone()]);

    c.mw.git(&["pull", "-q", "--ff-only", "origin", "main"]);
    let middle_sha = c.mw.head();
    run_ok(&c.mw.dir, &["push", "lib"]);

    assert_eq!(trailer(&c.leaf, "main", SOURCE), vec![middle_sha]);
    assert_eq!(
        trailer(&c.leaf, "main", "Signed-off-by"),
        vec!["Outer Dev <outer@example.test>".to_string()]
    );
    let all = c.leaf.git(&["log", "--format=%B", "main"]);
    assert!(
        !all.contains(&outer_sha),
        "outer's sha leaked into leaf:\n{all}"
    );

    run_ok(&c.mw.dir, &["doctor"]);
    c.mw.commit(
        "middle: second lib change",
        &[("lib/a.txt", Some("lib v2\n"))],
    );
    run_ok(&c.mw.dir, &["push", "lib"]);
    assert_eq!(trailer(&c.leaf, "main", SOURCE), vec![c.mw.head()]);
    assert!(run_ok(&c.mw.dir, &["status"]).stdout.contains("in sync"));
}

/// leaf -> middle -> outer. Outer's import of middle's import carries only outer's own
/// `Monosplice-Origin` (naming the middle commit), and `doctor` passes.
#[test]
fn s170_an_import_of_an_import_carries_only_its_own_origin() {
    let c = chain();
    let leaf_sha = c.leaf_commit_reaches_middle();
    let middle_sha = c.middle.git(&["rev-parse", "main"]);
    assert_eq!(trailer(&c.middle, "main", ORIGIN), vec![leaf_sha.clone()]);

    run_ok(&c.outer.dir, &["pull", "middle"]);
    assert_eq!(trailer(&c.outer, "HEAD", ORIGIN), vec![middle_sha]);
    assert!(!c.outer.git(&["log", "--format=%B"]).contains(&leaf_sha));
    assert_eq!(c.outer.subjects("HEAD").last().unwrap(), "leaf: add b");

    let doctor = run_ok(&c.outer.dir, &["doctor"]);
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );
    assert!(run_ok(&c.outer.dir, &["status"]).stdout.contains("in sync"));
}

/// `attach --import-history` replays middle's history, including middle's own import from the
/// leaf: every replayed commit carries exactly one `Monosplice-Origin`, and `doctor` passes
/// straight after the attach.
#[test]
fn s170_an_import_history_replay_carries_only_its_own_origins() {
    let c = chain();
    c.leaf_commit_reaches_middle();

    let outer2 = make_repo(c.sb.path(), "outer2");
    write_config(
        &outer2,
        &[&config_entry("middle", "vendor/middle", &c.middle_dir)],
    );
    outer2.commit("outer2: initial", &[("README", Some("outer2\n"))]);
    run_ok(
        &outer2.dir,
        &["attach", "vendor/middle", "--import-history"],
    );

    let replayed = outer2.git(&["rev-list", "HEAD", "--", "vendor/middle"]);
    let middle_log = c.middle.git(&["rev-list", "main"]);
    for sha in replayed.lines() {
        let origins = trailer(&outer2, sha, ORIGIN);
        assert_eq!(origins.len(), 1, "commit {sha} carries {origins:?}");
        assert!(middle_log.contains(&origins[0]), "{sha} names {origins:?}");
    }
    let doctor = run_ok(&outer2.dir, &["doctor"]);
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );
}
