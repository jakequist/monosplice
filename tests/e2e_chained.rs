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

// ---------------------------------------------------------------------------------------------
// S171: histories 1.0.0 already published with forwarded trailers keep working as they are.
// ---------------------------------------------------------------------------------------------

/// The chain of tests/fixtures/v1.0.0-chain, written by monosplice 1.0.0 (see generate.sh there):
/// relative remotes, so restoring the bundles side by side is all it takes.
struct Legacy {
    _sb: Sandbox,
    leaf: TestRepo,
    mw: TestRepo,
    outer: TestRepo,
    outer5: TestRepo,
}

fn legacy_chain() -> Legacy {
    let sb = sandbox();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1.0.0-chain");
    let restore = |name: &str, dest: &str, bare: bool| {
        let bundle = fixtures.join(format!("{name}.bundle"));
        let dest = sb.path().join(dest);
        let mut args = vec!["clone", "-q", "-b", "main"];
        if bare {
            args.push("--bare");
        }
        let bundle = bundle.to_string_lossy().into_owned();
        let dest_str = dest.to_string_lossy().into_owned();
        args.push(&bundle);
        args.push(&dest_str);
        TestRepo::new(sb.path()).git(&args);
        // A clone from a bundle remembers the bundle as `origin`; nothing here uses it.
        TestRepo::new(&dest)
    };
    let leaf = restore("leaf", "leaf.git", true);
    restore("middle", "middle.git", true);
    let mw = restore("mw", "mw", false);
    let outer = restore("outer", "outer", false);
    let outer5 = restore("outer5", "outer5", false);
    Legacy {
        _sb: sb,
        leaf,
        mw,
        outer,
        outer5,
    }
}

/// Bug 2's aftermath: the leaf's tip carries outer's `Monosplice-Source` ahead of middle's.
/// 1.0.0 read outer's sha as middle's broken mapping and refused every later push.
#[test]
fn s171_a_published_doubled_source_trailer_does_not_brick_the_middle() {
    let l = legacy_chain();
    let doubled = trailer(&l.leaf, "main", SOURCE);
    assert_eq!(doubled.len(), 2, "fixture: {doubled:?}");
    assert_eq!(doubled[1], l.mw.head(), "fixture: middle's own is last");

    let doctor = run_ok(&l.mw.dir, &["doctor"]);
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );
    assert!(
        doctor
            .stdout
            .contains("1 Monosplice-Source trailer(s) were forwarded"),
        "{}",
        doctor.stdout
    );
    assert!(run_ok(&l.mw.dir, &["status"]).stdout.contains("in sync"));
    assert!(run_ok(&l.mw.dir, &["pull", "--dry-run"])
        .stdout
        .contains("up to date"));

    l.mw.commit(
        "middle: after the upgrade",
        &[("lib/a.txt", Some("lib v1\nouter patch\nmiddle again\n"))],
    );
    run_ok(&l.mw.dir, &["push", "lib"]);
    assert_eq!(trailer(&l.leaf, "main", SOURCE), vec![l.mw.head()]);
    assert_eq!(
        l.leaf.tree_sha("main", None),
        l.mw.tree_sha("HEAD", Some("lib"))
    );
}

/// Bug 3's aftermath: an outer import carries the leaf's sha ahead of middle's. 1.0.0's
/// `doctor` reported the leaf sha as an import no configured remote has.
#[test]
fn s171_a_published_doubled_origin_trailer_is_not_an_orphaned_import() {
    let l = legacy_chain();
    let doubled = trailer(&l.outer, "HEAD~1", ORIGIN);
    assert_eq!(doubled.len(), 2, "fixture: {doubled:?}");

    let doctor = run_ok(&l.outer.dir, &["doctor"]);
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );
    assert!(
        doctor
            .stdout
            .contains("1 Monosplice-Origin trailer(s) in monorepo history were forwarded"),
        "{}",
        doctor.stdout
    );
    let status = run_ok(&l.outer.dir, &["status"]);
    assert!(status.stdout.contains("in sync"), "{}", status.stdout);
}

/// Bug 5's aftermath: an `--import-history` replay by 1.0.0 carries the same doubled Origin.
/// Only the last one is the replayed commit's claim, so the leaf sha ahead of it is not an
/// import this monorepo made — and not an orphan no configured remote has.
#[test]
fn s171_a_published_doubled_origin_from_an_import_history_replay() {
    let l = legacy_chain();
    let doubled = trailer(&l.outer5, "HEAD", ORIGIN);
    assert_eq!(doubled.len(), 2, "fixture: {doubled:?}");

    let json = run_monosplice(&l.outer5.dir, &["doctor", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&json.stdout).expect("json");
    assert_eq!(
        report["monorepo"]["problems"],
        serde_json::json!([]),
        "{}",
        json.stdout
    );
    assert_eq!(
        report["monorepo"]["notes"][0],
        "informational: 1 Monosplice-Origin trailer(s) in monorepo history were forwarded from an earlier hop — ignored."
    );
}

// ---------------------------------------------------------------------------------------------
// S172: vendoring a repository that another monorepo publishes.
// ---------------------------------------------------------------------------------------------

struct Published {
    sb: Sandbox,
    lib_dir: String,
    publisher: TestRepo,
    consumer: TestRepo,
}

/// `publisher` splices `lib/` out to lib.git; `consumer` is an unrelated monorepo with lib.git
/// configured at `vendor/lib` but not attached yet.
fn published() -> Published {
    let sb = sandbox();
    let root = sb.path();
    let lib_dir = make_bare_remote(root, "lib");

    let publisher = make_repo(root, "publisher");
    write_config(&publisher, &[&config_entry("lib", "lib", &lib_dir)]);
    publisher.commit("lib v1", &[("lib/a.txt", Some("v1\n"))]);
    run_ok(&publisher.dir, &["attach", "lib", "--yes"]);

    let consumer = make_repo(root, "consumer");
    write_config(&consumer, &[&config_entry("lib", "vendor/lib", &lib_dir)]);
    consumer.commit("consumer: initial", &[("README", Some("x\n"))]);
    Published {
        sb,
        lib_dir,
        publisher,
        consumer,
    }
}

/// Bugs 1a–1c: the publisher's trailers name commits the consumer never had. Before the fix
/// the consumer read them as its own exports: `attach <folder>` refused ("already
/// connected"), `doctor` and `push` refused (broken mapping), and upstream updates never
/// imported (status "in sync" while stale).
#[test]
fn s172_a_vendored_copy_of_a_published_repo_attaches_pulls_and_pushes() {
    let p = published();
    let consumer = &p.consumer;

    // 1c: the configured-entry form of attach, exactly like the url form.
    run_ok(&consumer.dir, &["attach", "vendor/lib"]);
    assert_eq!(consumer.read("vendor/lib/a.txt"), "v1\n");
    assert!(run_ok(&consumer.dir, &["status"])
        .stdout
        .contains("in sync"));

    // 1b: the publisher's trailers are not the consumer's mapping.
    let doctor = run_ok(&consumer.dir, &["doctor"]);
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );

    // 1a: an upstream release is standalone work to pull.
    p.publisher.commit("lib v2", &[("lib/a.txt", Some("v2\n"))]);
    run_ok(&p.publisher.dir, &["push"]);
    let status = run_ok(&consumer.dir, &["status"]);
    assert!(status.stdout.contains("1 to pull"), "{}", status.stdout);
    run_ok(&consumer.dir, &["pull"]);
    assert_eq!(consumer.read("vendor/lib/a.txt"), "v2\n");
    let lib = TestRepo::new(&p.lib_dir);
    assert_eq!(
        trailer(consumer, "HEAD", ORIGIN),
        vec![lib.git(&["rev-parse", "main"])]
    );
    assert!(trailer(consumer, "HEAD", SOURCE).is_empty());
    assert!(run_ok(&consumer.dir, &["status"])
        .stdout
        .contains("in sync"));

    // 1b: a patch goes back with the consumer's own trailer.
    consumer.commit(
        "consumer: patch lib",
        &[("vendor/lib/a.txt", Some("v2\nconsumer patch\n"))],
    );
    let dry = run_ok(&consumer.dir, &["push", "--dry-run"]);
    assert!(dry.stdout.contains("consumer: patch lib"), "{}", dry.stdout);
    run_ok(&consumer.dir, &["push"]);
    assert_eq!(trailer(&lib, "main", SOURCE), vec![consumer.head()]);
    assert!(run_ok(&consumer.dir, &["status"])
        .stdout
        .contains("in sync"));

    // ...and a later release on top of that patch still imports: the branch has shown another
    // publisher, so a claim the consumer cannot place above its own export is that publisher's.
    let lib_clone = clone_remote(p.sb.path(), &p.lib_dir, "lib-clone");
    lib_clone.commit(
        &format!("lib v3\n\n{SOURCE}: {}", "3".repeat(40)),
        &[("c.txt", Some("v3\n"))],
    );
    lib_clone.git(&["push", "-q", "origin", "main"]);
    let status = run_ok(&consumer.dir, &["status"]);
    assert!(status.stdout.contains("1 to pull"), "{}", status.stdout);
    run_ok(&consumer.dir, &["pull"]);
    assert_eq!(consumer.read("vendor/lib/c.txt"), "v3\n");
    assert!(run_ok(&consumer.dir, &["status"])
        .stdout
        .contains("in sync"));
}

/// Two monorepos vendor the same middle monorepo. The first pushed a patch into middle, so
/// middle's tip carries the first's `Monosplice-Source` — a commit the second never had (the
/// legacy fixture: `outer` pushed, `outer5` attached earlier). To the second that is middle's
/// ordinary history, to pull; 1.0.0 read it as the second's own broken mapping.
#[test]
fn s172_a_second_consumer_pulls_the_first_consumers_patch() {
    let l = legacy_chain();
    let doctor = run_ok(&l.outer5.dir, &["doctor"]);
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );
    assert!(doctor.stdout.contains("to pull: 1"), "{}", doctor.stdout);

    run_ok(&l.outer5.dir, &["pull"]);
    assert_eq!(
        l.outer5.read("vendor/middle/lib/a.txt"),
        "lib v1\nouter patch\n"
    );
    assert!(trailer(&l.outer5, "HEAD", SOURCE).is_empty());
    assert_eq!(trailer(&l.outer5, "HEAD", ORIGIN).len(), 1);
    assert!(run_ok(&l.outer5.dir, &["status"])
        .stdout
        .contains("in sync"));
}

// ---------------------------------------------------------------------------------------------
// S173: the outer monorepo vendors both middle and middle's leaf.
// ---------------------------------------------------------------------------------------------

/// Bug 1d: the leaf's `Monosplice-Source` trailers name middle commits, which *resolve* in the
/// outer repository because it fetched middle into `refs/monosplice/middle/remote`. They are
/// middle's commits, not outer's rewritten exports.
#[test]
fn s173_claims_naming_commits_fetched_from_another_subrepo_are_foreign() {
    let c = chain();
    write_config(
        &c.outer,
        &[
            &config_entry("middle", "vendor/middle", &c.middle_dir),
            &config_entry("leaf", "vendor/leaf", &c.leaf_dir),
        ],
    );
    c.outer.commit("outer: vendor the leaf too", &[]);
    run_ok(&c.outer.dir, &["attach", "vendor/leaf"]);
    c.outer.commit(
        "outer: patch leaf",
        &[("vendor/leaf/a.txt", Some("lib v1\nouter patch\n"))],
    );

    let doctor = run_ok(&c.outer.dir, &["doctor", "leaf"]);
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );
    for wrong in ["no longer an ancestor", "recovered", "rewritten"] {
        assert!(
            !doctor.stdout.contains(wrong),
            "{wrong}:\n{}",
            doctor.stdout
        );
    }
    let dry = run_ok(&c.outer.dir, &["push", "--dry-run", "leaf"]);
    assert!(dry.stdout.contains("outer: patch leaf"), "{}", dry.stdout);
    run_ok(&c.outer.dir, &["push", "leaf"]);
    assert_eq!(trailer(&c.leaf, "main", SOURCE), vec![c.outer.head()]);
}

// ---------------------------------------------------------------------------------------------
// S174: a claim that could still be ours stops, exactly as before.
// ---------------------------------------------------------------------------------------------

/// A shallow clone of the consumer cannot see beyond its boundary, so an unresolvable claim
/// could be one of its own exports: the old refusal and message stand.
#[test]
fn s174_a_shallow_clone_still_refuses_an_unresolvable_claim() {
    let p = published();
    run_ok(&p.consumer.dir, &["attach", "vendor/lib"]);
    p.consumer
        .commit("consumer: more", &[("README", Some("y\n"))]);

    let url = format!("file://{}", p.consumer.dir.display());
    let shallow = TestRepo::new(p.sb.path().join("shallow"));
    TestRepo::new(p.sb.path()).git(&["clone", "-q", "--depth", "1", &url, "shallow"]);
    shallow.commit(
        "consumer: patch lib",
        &[("vendor/lib/a.txt", Some("v1\npatch\n"))],
    );

    let push = run_monosplice(&shallow.dir, &["push"]);
    assert_ne!(push.exit_code, 0, "{}", push.stdout);
    assert!(
        push.stderr.contains("does not exist in this clone"),
        "{}",
        push.stderr
    );
    let doctor = run_monosplice(&shallow.dir, &["doctor"]);
    assert_eq!(doctor.exit_code, 1, "{}", doctor.stdout);
}

/// The consumer pushed a patch, then amended it (same author, same date, same subject) and
/// cloned afresh: the clone has no object for the sha its own export names. On a branch that
/// another monorepo also publishes, that claim is not read as the other publisher's — a commit
/// on the clone's history is recognisably the one exported — so it stops as a broken mapping
/// instead of being offered for pull.
#[test]
fn s174_our_own_rewritten_export_is_not_mistaken_for_the_other_publishers() {
    let p = published();
    let consumer = &p.consumer;
    run_ok(&consumer.dir, &["attach", "vendor/lib"]);
    consumer.commit(
        "consumer: patch lib",
        &[("vendor/lib/a.txt", Some("v1\npatch\n"))],
    );
    run_ok(&consumer.dir, &["push"]);
    consumer.git(&["commit", "-q", "--amend", "--no-edit", "--allow-empty"]);

    let url = format!("file://{}", consumer.dir.display());
    let fresh = clone_remote(p.sb.path(), &url, "fresh");
    let lib = TestRepo::new(&p.lib_dir);
    let exported_from = trailer(&lib, "main", SOURCE);
    assert_eq!(exported_from.len(), 1);
    assert_ne!(
        fresh
            .git_try(&["cat-file", "-e", &exported_from[0]])
            .exit_code,
        0,
        "the fresh clone must not have the amended-away commit"
    );

    let status = run_monosplice(&fresh.dir, &["status"]);
    assert!(!status.stdout.contains("to pull"), "{}", status.stdout);
    fresh.commit("consumer: more", &[("vendor/lib/b.txt", Some("b\n"))]);
    let push = run_monosplice(&fresh.dir, &["push"]);
    assert_ne!(push.exit_code, 0, "{}", push.stdout);
    assert!(
        push.stderr.contains(&exported_from[0])
            && push.stderr.contains("does not exist in this clone"),
        "{}",
        push.stderr
    );
}

/// Unchanged from 1.0.0, and the one case this rule does not open up: on a branch this monorepo
/// publishes, where no other monosplice publisher has shown up before, a claim naming a commit
/// it does not have could be its own export whose history it cannot see, so it stops (S52).
/// Here that is the consumer's patch, landed on the publisher's branch: the publisher cannot
/// tell it from a broken mapping and refuses to export over it. (The consumer side of the same
/// branch works: S172.)
#[test]
fn s174_a_publisher_still_stops_at_the_first_claim_it_cannot_place() {
    let p = published();
    run_ok(&p.consumer.dir, &["attach", "vendor/lib"]);
    p.consumer.commit(
        "consumer: patch lib",
        &[("vendor/lib/a.txt", Some("v1\npatch\n"))],
    );
    run_ok(&p.consumer.dir, &["push"]);
    let lib = TestRepo::new(&p.lib_dir);
    let pub_head = lib.git(&["rev-parse", "main"]);

    p.publisher
        .commit("lib v2", &[("lib/other.txt", Some("v2\n"))]);
    let push = run_monosplice(&p.publisher.dir, &["push"]);
    assert_ne!(push.exit_code, 0, "{}", push.stdout);
    assert!(
        push.stderr.contains(&p.consumer.head())
            && push.stderr.contains("does not exist in this clone"),
        "{}",
        push.stderr
    );
    assert_eq!(
        lib.git(&["rev-parse", "main"]),
        pub_head,
        "nothing was pushed"
    );
}

// ---------------------------------------------------------------------------------------------
// S175: doctor mentions foreign trailers as information.
// ---------------------------------------------------------------------------------------------

#[test]
fn s175_doctor_reports_another_monorepos_trailers_as_information() {
    let p = published();
    run_ok(&p.consumer.dir, &["attach", "vendor/lib"]);
    let doctor = run_ok(&p.consumer.dir, &["doctor"]);
    assert!(
        doctor.stdout.contains(
            "informational: 1 standalone commit(s) carry a Monosplice-Source trailer written by another monorepo — ignored."
        ),
        "{}",
        doctor.stdout
    );
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );

    let json = run_ok(&p.consumer.dir, &["doctor", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&json.stdout).expect("json");
    assert_eq!(report["ok"], true);
    assert_eq!(report["subrepos"][0]["problems"], serde_json::json!([]));
    assert_eq!(
        report["subrepos"][0]["notes"].as_array().map(Vec::len),
        Some(1)
    );
}
