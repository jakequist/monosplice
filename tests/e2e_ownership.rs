//! e2e (S177–S182): whose `Monosplice-Source` claim is it, decided on evidence.
//!
//! A claim on a standalone commit is this monorepo's export only when that can be shown: the
//! standalone commit's tree is exactly what the named commit publishes today, or (for a named
//! commit on HEAD's history) it sits directly on work both sides already agree on. It is another
//! monorepo's only when that can be shown too: it names a commit of a standalone repository
//! monosplice fetched, or it carries a `Monosplice-Monorepo` id this monorepo never had, or no id
//! while this monorepo has had one since its first config. Whatever sits at or below a commit this
//! monorepo imported, or below a verified export of its own, is settled. Everything else stops —
//! and says so in `status`, `pull`, `push` and `doctor`.
//!
//! The scenarios are the reviewer's reproductions of PR #2's defects (d1, d3, d4, d7, d8, d9, a1)
//! plus the commit-date ordering hole, as black-box tests.

mod common;

use std::path::Path;

use common::{
    clone_remote, make_bare_remote, make_repo, run_monosplice, sandbox, subrepo_block, toml_str,
    write_config, write_config_with_id, RunResult, Sandbox, TestRepo,
};

const SOURCE: &str = "Monosplice-Source";
const MONOREPO: &str = "Monosplice-Monorepo";

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

fn run_fails(dir: &Path, args: &[&str]) -> RunResult {
    let res = run_monosplice(dir, args);
    assert_ne!(
        res.exit_code,
        0,
        "`monosplice {}` should have refused in {}:\n{}\n{}",
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

fn entry(name: &str, path: &str, remote: &str) -> String {
    subrepo_block(&[
        ("name", &toml_str(name)),
        ("path", &toml_str(path)),
        ("remote", &toml_str(remote)),
    ])
}

/// A monorepo `id`, fixed per role so the tests read clearly.
const P_ID: &str = "publisher-0001";
const K_ID: &str = "consumer-0002";

/// `mono` splices `lib/` out to lib.git (published with a baseline commit).
struct Publisher {
    sb: Sandbox,
    lib_dir: String,
    lib: TestRepo,
    mono: TestRepo,
}

fn publisher(id: Option<&str>) -> Publisher {
    let sb = sandbox();
    let root = sb.path();
    let lib_dir = make_bare_remote(root, "lib");
    let mono = make_repo(root, "mono");
    let block = entry("lib", "lib", &lib_dir);
    match id {
        Some(id) => write_config_with_id(&mono, id, &[&block]),
        None => write_config(&mono, &[&block]),
    }
    mono.commit(
        "lib v1",
        &[("lib/a.txt", Some("v1\n")), ("app/x.txt", Some("app\n"))],
    );
    run_ok(&mono.dir, &["attach", "lib", "--yes"]);
    Publisher {
        lib: TestRepo::new(&lib_dir),
        sb,
        lib_dir,
        mono,
    }
}

impl Publisher {
    /// A monorepo that vendors lib.git at `vendor/lib` (snapshot attach).
    fn consumer(&self, name: &str, id: Option<&str>) -> TestRepo {
        let k = make_repo(self.sb.path(), name);
        let block = entry("lib", "vendor/lib", &self.lib_dir);
        match id {
            Some(id) => write_config_with_id(&k, id, &[&block]),
            None => write_config(&k, &[&block]),
        }
        k.commit(&format!("{name}: initial"), &[("README", Some("k\n"))]);
        run_ok(&k.dir, &["attach", "vendor/lib"]);
        k
    }

    fn contributor(&self, name: &str) -> TestRepo {
        clone_remote(self.sb.path(), &self.lib_dir, name)
    }

    fn lib_files(&self) -> Vec<String> {
        let out = self.lib.git(&["ls-tree", "-r", "--name-only", "main"]);
        out.lines().map(str::to_string).collect()
    }
}

fn assert_not_silent(status: &RunResult) {
    let out = format!("{}{}", status.stdout, status.stderr);
    assert!(!status.stdout.contains("in sync"), "{out}");
    assert!(status.stdout.contains("stopped"), "{out}");
    assert!(out.contains("monosplice doctor"), "{out}");
}

// ---------------------------------------------------------------------------------------------
// S177: a copied Monosplice-Source line never hides standalone work (d3, d4).
// ---------------------------------------------------------------------------------------------

/// d4: three contributor commits; the last carries the Source and id lines of an earlier export
/// (pasted, cherry-picked — every export publishes them). None of the three is ours.
#[test]
fn s177_a_copied_source_line_never_hides_the_commits_below_it() {
    let p = publisher(Some(P_ID));
    let lw = p.contributor("lw");
    let copied = lw.git(&[
        "log",
        "-1",
        "--format=%(trailers:key=Monosplice-Source,key=Monosplice-Monorepo)",
    ]);
    assert!(copied.contains(SOURCE), "{copied}");
    lw.commit("contrib 1", &[("c1.txt", Some("one\n"))]);
    lw.commit("contrib 2", &[("c2.txt", Some("two\n"))]);
    lw.commit(
        &format!("contrib 3\n\n{copied}"),
        &[("c3.txt", Some("three\n"))],
    );
    lw.git(&["push", "-q", "origin", "main"]);

    let status = run_ok(&p.mono.dir, &["status"]);
    assert!(status.stdout.contains("3 to pull"), "{}", status.stdout);
    run_ok(&p.mono.dir, &["pull"]);
    for f in ["c1", "c2", "c3"] {
        assert!(p.mono.exists(&format!("lib/{f}.txt")), "{f} not imported");
    }
    p.mono
        .commit("feat: more", &[("lib/a.txt", Some("v1\nmore\n"))]);
    run_ok(&p.mono.dir, &["push"]);
    let files = p.lib_files();
    for f in ["c1.txt", "c2.txt", "c3.txt"] {
        assert!(
            files.contains(&f.to_string()),
            "push dropped {f}: {files:?}"
        );
    }
    run_ok(&p.mono.dir, &["doctor"]);
}

/// d3: a maintainer reverts one of our exports, lands a human fix, then re-lands the export with
/// `git cherry-pick`, which copies its Source line. The fix is not hidden and survives the next
/// push; the re-land brings the export's content back.
#[test]
fn s177_a_cherry_picked_reland_does_not_hide_the_fix_before_it() {
    let p = publisher(Some(P_ID));
    p.mono
        .commit("feat: f", &[("lib/f.txt", Some("feature\n"))]);
    run_ok(&p.mono.dir, &["push"]);

    let lw = p.contributor("lw");
    lw.git(&["revert", "--no-edit", "HEAD"]);
    lw.commit(
        "fix: human fix on standalone",
        &[("h.txt", Some("human fix\n"))],
    );
    lw.git(&["cherry-pick", "HEAD~2"]);
    lw.git(&["push", "-q", "origin", "main"]);

    run_ok(&p.mono.dir, &["pull"]);
    assert_eq!(p.mono.read("lib/h.txt"), "human fix\n");
    assert_eq!(p.mono.read("lib/f.txt"), "feature\n");
    p.mono
        .commit("feat: more", &[("lib/a.txt", Some("v1\nmore\n"))]);
    run_ok(&p.mono.dir, &["push"]);
    let files = p.lib_files();
    assert!(files.contains(&"h.txt".to_string()), "{files:?}");
    assert!(files.contains(&"f.txt".to_string()), "{files:?}");
    assert_eq!(
        p.lib.tree_sha("main", None),
        p.mono.tree_sha("HEAD", Some("lib"))
    );
}

// ---------------------------------------------------------------------------------------------
// S178: an export of ours that this clone cannot see stops (d8, d9).
// ---------------------------------------------------------------------------------------------

/// A consumer with an id vendors an ordinary repository (no other publisher anywhere) and pushes
/// a patch; then the monorepo commit leaves its history. A fresh clone must stop.
fn own_export_lost(variant: &str) {
    let sb = sandbox();
    let root = sb.path();
    let lib_dir = make_bare_remote(root, "lib");
    let up = clone_remote(root, &lib_dir, "up");
    up.commit("upstream v1", &[("a.txt", Some("v1\n"))]);
    up.git(&["push", "-q", "origin", "HEAD:main"]);

    let origin_dir = make_bare_remote(root, "kacho");
    let k = make_repo(root, "kacho");
    write_config_with_id(&k, K_ID, &[&entry("lib", "vendor/lib", &lib_dir)]);
    k.commit("init", &[("README", Some("k\n"))]);
    run_ok(&k.dir, &["attach", "vendor/lib"]);
    k.git(&["remote", "add", "origin", &origin_dir]);
    k.git(&["push", "-q", "origin", "main"]);

    if variant == "squash" {
        k.git(&["checkout", "-q", "-b", "fix"]);
        k.commit(
            "kacho: patch lib",
            &[("vendor/lib/a.txt", Some("v1\nkacho patch\n"))],
        );
        run_ok(&k.dir, &["push"]);
        k.git(&["checkout", "-q", "main"]);
        k.git(&["merge", "-q", "--squash", "fix"]);
        k.commit("Patch lib (#7)", &[]);
        k.git(&["branch", "-q", "-D", "fix"]);
        k.git(&["push", "-q", "origin", "main"]);
    } else {
        k.commit(
            "kacho: bad patch",
            &[("vendor/lib/a.txt", Some("v1\nbad patch\n"))],
        );
        run_ok(&k.dir, &["push"]);
        k.git(&["reset", "-q", "--hard", "HEAD~1"]);
        k.git(&["push", "-q", "-f", "origin", "main"]);
    }
    let lib = TestRepo::new(&lib_dir);
    assert_eq!(trailer(&lib, "main", MONOREPO), vec![K_ID.to_string()]);
    let lost = trailer(&lib, "main", SOURCE);

    let fresh = clone_remote(root, &origin_dir, "fresh");
    let before = fresh.head();
    let status = run_ok(&fresh.dir, &["status"]);
    assert_not_silent(&status);
    let pull = run_fails(&fresh.dir, &["pull"]);
    assert!(pull.stderr.contains(&lost[0]), "{}", pull.stderr);
    assert!(
        pull.stderr.contains("does not exist in this clone"),
        "{}",
        pull.stderr
    );
    assert_eq!(fresh.head(), before, "{variant}: nothing may be imported");
    run_fails(&fresh.dir, &["pull", "--dry-run"]);
    fresh.commit("kacho: more", &[("vendor/lib/b.txt", Some("b\n"))]);
    let push = run_fails(&fresh.dir, &["push"]);
    assert!(push.stderr.contains(&lost[0]), "{}", push.stderr);
    let doctor = run_monosplice(&fresh.dir, &["doctor"]);
    assert_eq!(doctor.exit_code, 1, "{}", doctor.stdout);
    assert!(doctor.stdout.contains(&lost[0]), "{}", doctor.stdout);
    assert!(doctor.stdout.contains(K_ID), "{}", doctor.stdout);
}

#[test]
fn s178_a_fresh_clone_stops_at_an_export_whose_branch_was_squash_merged() {
    own_export_lost("squash");
}

#[test]
fn s178_a_fresh_clone_stops_at_an_export_that_was_dropped() {
    own_export_lost("drop");
}

/// d9: a contributor lands a commit whose Source names a commit of the standalone repository
/// itself. That is not an export of anybody's monorepo: it imports like any other contribution.
/// It does not change how a later dropped export of ours is read in a fresh clone: that stops.
#[test]
fn s178_a_contributors_source_line_is_ordinary_work_and_changes_nothing_else() {
    let p = publisher(Some(P_ID));
    let origin_dir = make_bare_remote(p.sb.path(), "mono-origin");
    p.mono.git(&["remote", "add", "origin", &origin_dir]);
    p.mono
        .commit("feat: good", &[("lib/good.txt", Some("ok\n"))]);
    run_ok(&p.mono.dir, &["push"]);
    p.mono.git(&["push", "-q", "origin", "main"]);

    let lw = p.contributor("lw");
    let own = lw.git(&["rev-parse", "HEAD~1"]);
    lw.commit(
        &format!("docs: contributor\n\n{SOURCE}: {own}"),
        &[("c.txt", Some("c\n"))],
    );
    lw.git(&["push", "-q", "origin", "main"]);
    run_ok(&p.mono.dir, &["pull"]);
    assert_eq!(p.mono.read("lib/c.txt"), "c\n");
    p.mono.git(&["push", "-q", "origin", "main"]);

    p.mono
        .commit("feat: bad", &[("lib/bad.txt", Some("secret\n"))]);
    run_ok(&p.mono.dir, &["push"]);
    p.mono.git(&["reset", "-q", "--hard", "HEAD~1"]);
    p.mono.git(&["push", "-q", "-f", "origin", "main"]);

    let fresh = clone_remote(p.sb.path(), &origin_dir, "fresh");
    let status = run_ok(&fresh.dir, &["status"]);
    assert_not_silent(&status);
    run_fails(&fresh.dir, &["pull"]);
    assert!(!fresh.exists("lib/bad.txt"), "a dropped export came back");
}

// ---------------------------------------------------------------------------------------------
// S179: two publishers, each with its own id (d1).
// ---------------------------------------------------------------------------------------------

#[test]
fn s179_two_publishers_import_each_others_exports() {
    let p = publisher(Some(P_ID));
    let k = p.consumer("kacho", Some(K_ID));
    k.commit(
        "kacho: fix lib",
        &[("vendor/lib/a.txt", Some("v1\nkacho fix\n"))],
    );
    run_ok(&k.dir, &["push"]);
    assert_eq!(trailer(&p.lib, "main", SOURCE), vec![k.head()]);
    assert_eq!(trailer(&p.lib, "main", MONOREPO), vec![K_ID.to_string()]);

    let status = run_ok(&p.mono.dir, &["status"]);
    assert!(status.stdout.contains("1 to pull"), "{}", status.stdout);
    let doctor = run_ok(&p.mono.dir, &["doctor"]);
    assert!(
        doctor.stdout.contains("all checks passed"),
        "{}",
        doctor.stdout
    );
    assert!(!doctor.stdout.contains("--unshallow"), "{}", doctor.stdout);
    run_ok(&p.mono.dir, &["pull"]);
    assert_eq!(p.mono.read("lib/a.txt"), "v1\nkacho fix\n");
    assert!(run_ok(&p.mono.dir, &["status"]).stdout.contains("in sync"));

    p.mono
        .commit("prod: change", &[("lib/b.txt", Some("p2\n"))]);
    run_ok(&p.mono.dir, &["push"]);
    assert_eq!(trailer(&p.lib, "main", SOURCE), vec![p.mono.head()]);
    assert_eq!(trailer(&p.lib, "main", MONOREPO), vec![P_ID.to_string()]);
    run_ok(&p.mono.dir, &["doctor"]);

    run_ok(&k.dir, &["pull"]);
    assert_eq!(k.read("vendor/lib/b.txt"), "p2\n");
    assert!(run_ok(&k.dir, &["status"]).stdout.contains("in sync"));
    run_ok(&k.dir, &["doctor"]);

    // Proven by content: all three agree.
    let tree = p.lib.tree_sha("main", None);
    assert_eq!(p.mono.tree_sha("HEAD", Some("lib")), tree);
    assert_eq!(k.tree_sha("HEAD", Some("vendor/lib")), tree);
}

// ---------------------------------------------------------------------------------------------
// S180: authorship is never evidence (d7, a1).
// ---------------------------------------------------------------------------------------------

/// d7: the consumer applies the publisher's fix early with `git am`, which keeps author, date and
/// subject. The publisher then publishes the fix and a follow-up.
#[test]
fn s180_a_consumer_that_applied_an_upstream_fix_with_git_am_keeps_syncing() {
    let p = publisher(Some(P_ID));
    let k = p.consumer("kacho", Some(K_ID));
    p.mono
        .commit("fix: typo", &[("lib/fix.txt", Some("fixed\n"))]);
    let patch = p
        .mono
        .git(&["format-patch", "-1", "--relative=lib", "--stdout"]);
    k.git_with(&["am", "-q", "--directory=vendor/lib"], &[], Some(&patch));
    p.mono
        .commit("fix: follow-up", &[("lib/fix.txt", Some("fixed\nmore\n"))]);
    run_ok(&p.mono.dir, &["push"]);

    let status = run_ok(&k.dir, &["status"]);
    assert!(status.stdout.contains("2 to pull"), "{}", status.stdout);
    run_ok(&k.dir, &["pull"]);
    assert_eq!(k.read("vendor/lib/fix.txt"), "fixed\nmore\n");
    run_ok(&k.dir, &["doctor"]);
    k.commit("kacho: patch", &[("vendor/lib/k.txt", Some("k\n"))]);
    run_ok(&k.dir, &["push"]);
    assert_eq!(trailer(&p.lib, "main", SOURCE), vec![k.head()]);
}

/// a1: production is the only publisher of main; the consumer vendors it in fork mode and the
/// maintainer lands the consumer's fork patch in production with `git am`.
fn fork_patch_landed_with_git_am(am_opts: &[&str]) {
    let p = publisher(Some(P_ID));
    let fork_dir = make_bare_remote(p.sb.path(), "fork");
    let k = make_repo(p.sb.path(), "kacho");
    write_config_with_id(
        &k,
        K_ID,
        &[&subrepo_block(&[
            ("name", &toml_str("lib")),
            ("path", &toml_str("vendor/lib")),
            ("remote", &toml_str(&fork_dir)),
            ("upstream", &toml_str(&p.lib_dir)),
            ("push-branch", &toml_str("main")),
        ])],
    );
    k.commit("init", &[("README", Some("k\n"))]);
    run_ok(&k.dir, &["attach", "vendor/lib"]);
    k.commit(
        "kacho: fix lib",
        &[("vendor/lib/a.txt", Some("v1\nkacho fix\n"))],
    );
    let ksha = k.head();
    run_ok(&k.dir, &["push"]);

    let fork = TestRepo::new(&fork_dir);
    let patch = fork.git(&["format-patch", "-1", "--stdout", "main"]);
    let mut am = vec!["am", "-q"];
    am.extend_from_slice(am_opts);
    am.push("--directory=lib");
    p.mono.git_with(&am, &[], Some(&patch));
    run_ok(&p.mono.dir, &["push"]);
    let published = p.lib.git(&["log", "--format=%B", "main"]);
    assert!(
        !published.contains(&ksha),
        "consumer sha leaked:\n{published}"
    );
    assert!(
        !published.contains(K_ID),
        "consumer id leaked:\n{published}"
    );
    p.mono.commit("prod: more", &[("lib/b.txt", Some("p2\n"))]);
    run_ok(&p.mono.dir, &["push"]);
    run_ok(&p.mono.dir, &["doctor"]);

    run_ok(&k.dir, &["pull"]);
    assert!(run_ok(&k.dir, &["status"]).stdout.contains("in sync"));
    run_ok(&k.dir, &["doctor"]);
    assert_eq!(k.read("vendor/lib/b.txt"), "p2\n");
    k.commit(
        "kacho: second",
        &[("vendor/lib/a.txt", Some("v1\nkacho fix\nk2\n"))],
    );
    run_ok(&k.dir, &["push"]);
    assert_eq!(trailer(&fork, "main", SOURCE), vec![k.head()]);
}

#[test]
fn s180_the_fork_arrangement_with_plain_git_am() {
    fork_patch_landed_with_git_am(&[]);
}

#[test]
fn s180_the_fork_arrangement_with_git_am_ignore_date() {
    fork_patch_landed_with_git_am(&["--ignore-date"]);
}

// ---------------------------------------------------------------------------------------------
// S181: claims without an id.
// ---------------------------------------------------------------------------------------------

/// The publisher has no id (a 1.0.0-era monorepo). A consumer that has had one since its first
/// config never wrote an id-less claim, so the publisher's releases are the publisher's.
#[test]
fn s181_a_monorepo_with_an_id_from_the_start_imports_an_id_less_publishers_releases() {
    let p = publisher(None);
    let k = p.consumer("kacho", Some(K_ID));
    p.mono.commit("lib v2", &[("lib/a.txt", Some("v2\n"))]);
    run_ok(&p.mono.dir, &["push"]);
    assert!(trailer(&p.lib, "main", MONOREPO).is_empty());

    let status = run_ok(&k.dir, &["status"]);
    assert!(status.stdout.contains("1 to pull"), "{}", status.stdout);
    run_ok(&k.dir, &["pull"]);
    assert_eq!(k.read("vendor/lib/a.txt"), "v2\n");
    run_ok(&k.dir, &["doctor"]);
}

/// Neither side has an id: the consumer cannot tell the release from an export of its own that
/// it cannot see. 1.0.0 said "in sync" and "up to date"; now every command says what it found.
#[test]
fn s181_without_an_id_an_unplaceable_claim_is_reported_not_skipped() {
    let p = publisher(None);
    // Attaching is first contact whatever the claims say: its snapshot settles them.
    let k = p.consumer("kacho", None);
    assert_eq!(k.read("vendor/lib/a.txt"), "v1\n");
    assert_eq!(
        k.tree_sha("HEAD", Some("vendor/lib")),
        p.lib.tree_sha("main", None)
    );
    assert!(run_ok(&k.dir, &["status"]).stdout.contains("in sync"));
    run_ok(&k.dir, &["doctor"]);
    p.mono.commit("lib v2", &[("lib/a.txt", Some("v2\n"))]);
    run_ok(&p.mono.dir, &["push"]);
    let claim = trailer(&p.lib, "main", SOURCE);

    let status = run_ok(&k.dir, &["status"]);
    assert_not_silent(&status);
    let before = k.head();
    let pull = run_fails(&k.dir, &["pull"]);
    assert!(pull.stderr.contains(&claim[0]), "{}", pull.stderr);
    assert!(pull.stderr.contains(MONOREPO), "{}", pull.stderr);
    assert!(!pull.stdout.contains("up to date"), "{}", pull.stdout);
    assert_eq!(k.head(), before);
    let doctor = run_monosplice(&k.dir, &["doctor"]);
    assert_eq!(doctor.exit_code, 1, "{}", doctor.stdout);
    assert!(doctor.stdout.contains(&claim[0]), "{}", doctor.stdout);
    assert!(!doctor.stdout.contains("--unshallow"), "{}", doctor.stdout);

    // Adding an id later does not help: the monorepo may have written id-less claims before.
    write_config_with_id(&k, K_ID, &[&entry("lib", "vendor/lib", &p.lib_dir)]);
    k.commit("kacho: add an id", &[]);
    run_fails(&k.dir, &["pull"]);
}

// ---------------------------------------------------------------------------------------------
// S182: ancestry, not commit dates.
// ---------------------------------------------------------------------------------------------

/// A dead claim of ours on a side branch merged after our live export, dated before it. By date
/// it sits "below" the live export; by ancestry it does not.
#[test]
fn s182_a_dead_claim_on_a_merged_side_branch_is_not_superseded_by_date() {
    let p = publisher(Some(P_ID));
    let lw = p.contributor("lw");
    let base = lw.head();

    // The side branch: a commit claiming an export of ours this clone has never had, dated in
    // the past.
    lw.git(&["checkout", "-q", "-b", "side", &base]);
    lw.write("side.txt", "side\n");
    lw.git(&["add", "-A"]);
    let old = "1700000000 +0000";
    lw.git_with(
        &[
            "commit",
            "-q",
            "-m",
            &format!(
                "side work\n\n{SOURCE}: {}\n{MONOREPO}: {P_ID}",
                "d".repeat(40)
            ),
        ],
        &[("GIT_AUTHOR_DATE", old), ("GIT_COMMITTER_DATE", old)],
        None,
    );
    lw.git(&["push", "-q", "origin", "side"]);

    // Our live export lands on main.
    p.mono
        .commit("feat: live", &[("lib/live.txt", Some("live\n"))]);
    run_ok(&p.mono.dir, &["push"]);

    // The side branch is merged on the standalone repo afterwards.
    lw.git(&["checkout", "-q", "main"]);
    lw.git(&["pull", "-q", "--ff-only", "origin", "main"]);
    lw.git(&["merge", "-q", "--no-ff", "--no-edit", "side"]);
    lw.git(&["push", "-q", "origin", "main"]);

    let status = run_ok(&p.mono.dir, &["status"]);
    assert_not_silent(&status);
    run_fails(&p.mono.dir, &["pull"]);
    assert!(!p.mono.exists("lib/side.txt"));
    p.mono
        .commit("feat: more", &[("lib/more.txt", Some("more\n"))]);
    run_fails(&p.mono.dir, &["push"]);
    let doctor = run_monosplice(&p.mono.dir, &["doctor"]);
    assert_eq!(doctor.exit_code, 1, "{}", doctor.stdout);
}
