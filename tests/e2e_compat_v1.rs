//! e2e (S176): monosplice 1.0.0 histories keep working, and what is written now for an
//! ordinary single-hop setup is exactly what 1.0.0 wrote.
//!
//! Every commit in [`single_hop`] has a fixed date and every remote is a relative path, so the
//! whole history is a pure function of the binary that drives it. Two consequences:
//!
//! - [`single_hop_history_is_the_one_v1_wrote`] runs in CI with no 1.0.0 binary at hand: it
//!   drives the scenario with the binary under test and compares every resulting sha with the
//!   shas monosplice 1.0.0 produced from the same steps. Equal shas mean byte-identical
//!   commits — same trees, messages and trailers — at every step, including the pushes and
//!   pulls made on top of history that 1.0.0 would have written. 1.0.0 therefore reads a
//!   repository synced by this version exactly as it reads its own.
//! - [`cross_checked_against_the_released_v1_binary`] re-derives those expectations live when
//!   `MONOSPLICE_V1_BIN` names a 1.0.0 binary, and also hands each binary the other one's
//!   history. It skips (loudly) when the variable is unset.
//!
//! If the scenario changes, regenerate the constants with
//! `MONOSPLICE_V1_BIN=/path/to/monosplice-1.0.0 cargo test --test e2e_compat_v1 -- --nocapture`,
//! which prints them.
//!
//! One single-hop commit is deliberately *not* byte-identical: the export of a conflicted
//! import. The import commit carries `Monosplice-Origin`, and 1.0.0 copied it into the public
//! commit ahead of its own `Monosplice-Source`; that line is no longer forwarded.
//! [`conflicted_import`] pins the difference to exactly that line and, with
//! `MONOSPLICE_V1_BIN`, shows each binary carrying on from the other's version of it.
//!
//! Histories 1.0.0 wrote across *two* hops — forwarded and doubled trailers — cannot be
//! reproduced by the current binary at all; those are restored from bundles 1.0.0 produced
//! (tests/e2e_chained.rs, S171).

mod common;

use std::cell::Cell;
use std::path::{Path, PathBuf};

use common::{
    clone_remote, make_bare_remote, make_repo, monosplice_bin, released_v1_bin, run_binary_env,
    sandbox, subrepo_block, toml_str, write_config, RunResult, TestRepo,
};

/// What 1.0.0 produced from [`single_hop`]: (step, monorepo head, core head, lib head).
const V1_SHAS: &[(&str, &str, &str, &str)] = &[
    (
        "publish core",
        "bf3761308936ecb1c02270554f425b5fd7542983",
        "ea45437fe20591cdd30c41fe17cefedb214fb4e8",
        "d0b796ce61f0b2c5a50279009b2761a082781103",
    ),
    (
        "export",
        "ceaede8d0d20b0179c2f2a2aa5f5c69a3aa2eabf",
        "400ab69bf90e06bd88dfb9e0e0a3db38705a4ec3",
        "d0b796ce61f0b2c5a50279009b2761a082781103",
    ),
    (
        "import",
        "8d1557ad8fe63500b36442b19a155a8e273cd6b4",
        "0c3f6e12e291a6f87695bf988253b72f7a0a9b49",
        "d0b796ce61f0b2c5a50279009b2761a082781103",
    ),
    (
        "export with a trailer block",
        "6e08e2de02b88678a9d8fc4f71f38d4f6d92c99f",
        "69721b4441562b97a4f6df2333e8257c28200d64",
        "d0b796ce61f0b2c5a50279009b2761a082781103",
    ),
    (
        "vendor snapshot",
        "76b2e2e377292602403ed9c329a544e36b6ac339",
        "69721b4441562b97a4f6df2333e8257c28200d64",
        "d0b796ce61f0b2c5a50279009b2761a082781103",
    ),
    (
        "vendor update",
        "50ad22b8e9039682407494c25192cf19eb0712d2",
        "69721b4441562b97a4f6df2333e8257c28200d64",
        "dde8b6146c43c87ab636b957d5bcc5f7d1bea0bf",
    ),
    (
        "vendor patch pushed back",
        "739fb3d9beec418e43ae3c8a2e20f78717e0fd69",
        "69721b4441562b97a4f6df2333e8257c28200d64",
        "4e9aa97b43bf1559368d498658aab501a7cb81bf",
    ),
];

/// `status --json` as 1.0.0 printed it at the end of [`single_hop`].
const V1_STATUS_JSON: &str = "{\"subrepos\":[{\"name\":\"core\",\"path\":\"core\",\"remote\":\"../core.git\",\"branch\":\"main\",\"pullInProgress\":false,\"seeded\":true,\"ahead\":0,\"behind\":0,\"inSync\":true},{\"name\":\"lib\",\"path\":\"vendor/lib\",\"remote\":\"../lib.git\",\"branch\":\"main\",\"pullInProgress\":false,\"seeded\":true,\"ahead\":0,\"behind\":0,\"inSync\":true}]}\n";

/// Drives one binary through the scenario with a private clock, so the dates (and therefore
/// the shas) do not depend on which other tests ran first.
struct Script {
    bin: PathBuf,
    root: PathBuf,
    clock: Cell<u64>,
}

impl Script {
    fn new(bin: PathBuf, root: &Path) -> Self {
        Script {
            bin,
            root: root.to_path_buf(),
            clock: Cell::new(0),
        }
    }

    fn date(&self) -> String {
        let n = self.clock.get() + 1;
        self.clock.set(n);
        format!("{} +0000", 1_780_000_000 + n * 61)
    }

    fn ms(&self, repo: &TestRepo, args: &[&str]) -> RunResult {
        let date = self.date();
        run_binary_env(
            &self.bin,
            &repo.dir,
            args,
            &[
                ("GIT_AUTHOR_DATE", date.as_str()),
                ("GIT_COMMITTER_DATE", date.as_str()),
            ],
        )
    }

    fn ms_ok(&self, repo: &TestRepo, args: &[&str]) -> RunResult {
        let res = self.ms(repo, args);
        assert_eq!(
            res.exit_code,
            0,
            "{} `monosplice {}` failed:\n{}\n{}",
            self.bin.display(),
            args.join(" "),
            res.stdout,
            res.stderr
        );
        res
    }

    fn commit(&self, repo: &TestRepo, message: &str, files: &[(&str, &str)]) -> String {
        self.commit_as(repo, message, files, "Mono Author", "mono@example.test")
    }

    fn commit_as(
        &self,
        repo: &TestRepo,
        message: &str,
        files: &[(&str, &str)],
        name: &str,
        email: &str,
    ) -> String {
        for (rel, content) in files {
            repo.write(rel, content);
        }
        repo.git(&["add", "-A"]);
        let date = self.date();
        repo.git_with(
            &["commit", "-q", "-m", message],
            &[
                ("GIT_AUTHOR_DATE", date.as_str()),
                ("GIT_COMMITTER_DATE", date.as_str()),
                ("GIT_AUTHOR_NAME", name),
                ("GIT_AUTHOR_EMAIL", email),
            ],
            None,
        );
        repo.head()
    }
}

struct Repos {
    mono: TestRepo,
    core_pub: TestRepo,
    lib_pub: TestRepo,
    core_ext: TestRepo,
    lib_upstream: TestRepo,
}

struct Record {
    /// (step, monorepo head, core head, lib head) after each step.
    shas: Vec<(String, String, String, String)>,
    status_json: String,
}

fn head_or_none(repo: &TestRepo) -> String {
    let res = repo.git_try(&["rev-parse", "--verify", "-q", "main"]);
    if res.exit_code == 0 {
        res.stdout
    } else {
        "-".to_string()
    }
}

/// The ordinary single-hop life of a monorepo, as the owner runs it today: a directory
/// published with a baseline, exported and imported in both directions, and a third-party repo
/// vendored with a snapshot attach, updated from upstream and patched locally.
fn single_hop(s: &Script) -> (Repos, Record) {
    let root = s.root.as_path();
    make_bare_remote(root, "core");
    make_bare_remote(root, "lib");

    // The vendored project has history of its own before anyone attaches it.
    let lib_upstream = clone_remote(root, "lib.git", "lib-upstream");
    s.commit_as(
        &lib_upstream,
        "lib: first release",
        &[("README.md", "# lib\n"), ("src/lib.txt", "v1\n")],
        "Lib Maintainer",
        "lib@example.test",
    );
    lib_upstream.git(&["push", "-q", "origin", "HEAD:main"]);

    let mono = make_repo(root, "mono");
    write_config(
        &mono,
        &[
            &subrepo_block(&[
                ("path", &toml_str("core")),
                ("remote", &toml_str("../core.git")),
            ]),
            &subrepo_block(&[
                ("name", &toml_str("lib")),
                ("path", &toml_str("vendor/lib")),
                ("remote", &toml_str("../lib.git")),
            ]),
        ],
    );
    s.commit(
        &mono,
        "chore: initial monorepo",
        &[
            ("core/README.md", "# core\n"),
            ("core/src/index.txt", "hello\n"),
            ("app/main.txt", "private\n"),
        ],
    );

    let core_pub = TestRepo::new(root.join("core.git"));
    let lib_pub = TestRepo::new(root.join("lib.git"));
    let mut shas = Vec::new();
    let mut snap = |step: &str, mono: &TestRepo| {
        shas.push((
            step.to_string(),
            mono.head(),
            head_or_none(&core_pub),
            head_or_none(&lib_pub),
        ));
    };

    s.ms_ok(&mono, &["attach", "core", "--yes"]);
    snap("publish core", &mono);

    s.commit(&mono, "feat: two", &[("core/src/two.txt", "2\n")]);
    s.ms_ok(&mono, &["push", "core"]);
    snap("export", &mono);

    let core_ext = clone_remote(root, "core.git", "core-ext");
    s.commit_as(
        &core_ext,
        "feat: from a contributor",
        &[("CONTRIB.md", "outside work\n")],
        "Ext Contributor",
        "ext@example.test",
    );
    core_ext.git(&["push", "-q", "origin", "HEAD:main"]);
    s.ms_ok(&mono, &["pull", "core"]);
    snap("import", &mono);

    s.commit(
        &mono,
        "feat: three\n\nA body paragraph.\n\nSigned-off-by: Mono Author <mono@example.test>",
        &[
            ("core/src/three.txt", "3\n"),
            ("app/main.txt", "private 2\n"),
        ],
    );
    s.ms_ok(&mono, &["push", "core"]);
    snap("export with a trailer block", &mono);

    s.ms_ok(&mono, &["attach", "vendor/lib"]);
    snap("vendor snapshot", &mono);

    s.commit_as(
        &lib_upstream,
        "lib: second release",
        &[("src/lib.txt", "v2\n")],
        "Lib Maintainer",
        "lib@example.test",
    );
    lib_upstream.git(&["push", "-q", "origin", "HEAD:main"]);
    s.ms_ok(&mono, &["pull", "lib"]);
    snap("vendor update", &mono);

    s.commit(
        &mono,
        "fix: patch the vendored lib",
        &[("vendor/lib/src/lib.txt", "v2 patched\n")],
    );
    s.ms_ok(&mono, &["push", "lib"]);
    snap("vendor patch pushed back", &mono);

    let status_json = s.ms_ok(&mono, &["status", "--json"]).stdout;
    (
        Repos {
            mono,
            core_pub,
            lib_pub,
            core_ext,
            lib_upstream,
        },
        Record { shas, status_json },
    )
}

/// One more round in both directions on an existing history, driven by `s`.
fn another_round(s: &Script, repos: &Repos) {
    s.commit(&repos.mono, "feat: four", &[("core/src/four.txt", "4\n")]);
    s.ms_ok(&repos.mono, &["push"]);

    repos
        .core_ext
        .git(&["pull", "-q", "--ff-only", "origin", "main"]);
    s.commit_as(
        &repos.core_ext,
        "docs: from a contributor again",
        &[("CONTRIB.md", "more outside work\n")],
        "Ext Contributor",
        "ext@example.test",
    );
    repos.core_ext.git(&["push", "-q", "origin", "HEAD:main"]);
    s.ms_ok(&repos.mono, &["pull"]);

    repos
        .lib_upstream
        .git(&["pull", "-q", "--ff-only", "origin", "main"]);
    s.commit_as(
        &repos.lib_upstream,
        "lib: third release",
        &[("README.md", "# lib, third release\n")],
        "Lib Maintainer",
        "lib@example.test",
    );
    repos
        .lib_upstream
        .git(&["push", "-q", "origin", "HEAD:main"]);
    s.ms_ok(&repos.mono, &["pull", "lib"]);
    s.ms_ok(&repos.mono, &["push", "lib"]);
}

fn assert_in_sync(s: &Script, repos: &Repos) {
    let status = s.ms_ok(&repos.mono, &["status"]);
    assert_eq!(
        status.stdout.matches("in sync").count(),
        2,
        "{} status:\n{}",
        s.bin.display(),
        status.stdout
    );
    s.ms_ok(&repos.mono, &["doctor"]);
    assert_eq!(
        repos.core_pub.tree_sha("main", None),
        repos.mono.tree_sha("HEAD", Some("core"))
    );
    assert_eq!(
        repos.lib_pub.tree_sha("main", None),
        repos.mono.tree_sha("HEAD", Some("vendor/lib"))
    );
}

#[test]
fn single_hop_history_is_the_one_v1_wrote() {
    let sb = sandbox();
    let s = Script::new(monosplice_bin(), sb.path());
    let (_repos, record) = single_hop(&s);

    let got: Vec<(&str, &str, &str, &str)> = record
        .shas
        .iter()
        .map(|(a, b, c, d)| (a.as_str(), b.as_str(), c.as_str(), d.as_str()))
        .collect();
    assert_eq!(got, V1_SHAS);
    assert_eq!(record.status_json, V1_STATUS_JSON);
}

#[test]
fn cross_checked_against_the_released_v1_binary() {
    let Some(v1) = released_v1_bin() else {
        eprintln!("skipped: set MONOSPLICE_V1_BIN to a monosplice 1.0.0 binary to run this");
        return;
    };

    // The same steps through each binary produce the same repositories, byte for byte.
    let sb_v1 = sandbox();
    let s_v1 = Script::new(v1.clone(), sb_v1.path());
    let (v1_repos, v1_record) = single_hop(&s_v1);

    let sb_new = sandbox();
    let s_new = Script::new(monosplice_bin(), sb_new.path());
    let (new_repos, new_record) = single_hop(&s_new);

    println!("const V1_SHAS: &[(&str, &str, &str, &str)] = &[");
    for (step, mono, core, lib) in &v1_record.shas {
        println!("    ({step:?}, {mono:?}, {core:?}, {lib:?}),");
    }
    println!("];");
    println!("const V1_STATUS_JSON: &str = {:?};", v1_record.status_json);

    assert_eq!(new_record.shas, v1_record.shas);
    assert_eq!(new_record.status_json, v1_record.status_json);

    // A history 1.0.0 wrote, carried on by this version...
    let s = Script {
        bin: monosplice_bin(),
        root: sb_v1.path().to_path_buf(),
        clock: Cell::new(s_v1.clock.get()),
    };
    assert_in_sync(&s, &v1_repos);
    another_round(&s, &v1_repos);
    assert_in_sync(&s, &v1_repos);

    // ...and a history this version wrote, carried on by 1.0.0.
    let s = Script {
        bin: v1,
        root: sb_new.path().to_path_buf(),
        clock: Cell::new(s_new.clock.get()),
    };
    assert_in_sync(&s, &new_repos);
    another_round(&s, &new_repos);
    assert_in_sync(&s, &new_repos);
}

/// A mono edit and a standalone edit to the same line, resolved through the conflict flow and
/// pushed: the resolution is an import commit (`Monosplice-Origin`) that differs from its
/// origin, so it must export. Returns (monorepo, core remote, the resolution commit).
fn conflicted_import(s: &Script) -> (TestRepo, TestRepo, TestRepo, String) {
    let root = s.root.as_path();
    make_bare_remote(root, "core");
    let mono = make_repo(root, "mono");
    write_config(
        &mono,
        &[&subrepo_block(&[
            ("path", &toml_str("core")),
            ("remote", &toml_str("../core.git")),
        ])],
    );
    s.commit(&mono, "chore: initial", &[("core/a.txt", "base\n")]);
    s.ms_ok(&mono, &["attach", "core", "--yes"]);

    let ext = clone_remote(root, "core.git", "core-ext");
    s.commit_as(
        &ext,
        "feat: theirs",
        &[("a.txt", "theirs\n")],
        "Ext Contributor",
        "ext@example.test",
    );
    ext.git(&["push", "-q", "origin", "HEAD:main"]);
    s.commit(&mono, "feat: ours", &[("core/a.txt", "ours\n")]);

    let pull = s.ms(&mono, &["pull"]);
    assert_ne!(pull.exit_code, 0, "expected a conflict: {}", pull.stdout);
    mono.write("core/a.txt", "resolved\n");
    mono.git(&["add", "core/a.txt"]);
    s.ms_ok(&mono, &["pull", "--continue"]);
    let resolution = mono.head();
    s.ms_ok(&mono, &["push"]);
    (mono, TestRepo::new(root.join("core.git")), ext, resolution)
}

/// One more round in both directions on the conflicted-import history.
fn conflicted_round(s: &Script, mono: &TestRepo, ext: &TestRepo) {
    s.commit(mono, "feat: later", &[("core/later.txt", "later\n")]);
    s.ms_ok(mono, &["push"]);
    ext.git(&["pull", "-q", "--ff-only", "origin", "main"]);
    s.commit_as(
        ext,
        "docs: later",
        &[("LATER.md", "later\n")],
        "Ext Contributor",
        "ext@example.test",
    );
    ext.git(&["push", "-q", "origin", "HEAD:main"]);
    s.ms_ok(mono, &["pull"]);
    let status = s.ms_ok(mono, &["status"]);
    assert!(status.stdout.contains("in sync"), "{}", status.stdout);
    s.ms_ok(mono, &["doctor"]);
}

#[test]
fn the_export_of_a_conflicted_import_no_longer_forwards_its_origin() {
    let sb = sandbox();
    let s = Script::new(monosplice_bin(), sb.path());
    let (mono, core_pub, _ext, resolution) = conflicted_import(&s);

    let import_message = mono.git(&["log", "-1", "--format=%B", &resolution]);
    let exported_message = core_pub.git(&["log", "-1", "--format=%B", "main"]);
    let origin_line = import_message
        .lines()
        .find(|l| l.starts_with("Monosplice-Origin: "))
        .expect("the resolution is an import")
        .to_string();
    // 1.0.0 wrote the import's message, its Origin line, then the Source line. Now the Origin
    // line is gone and nothing else moved.
    let source_line = format!("Monosplice-Source: {resolution}");
    assert_eq!(
        exported_message,
        import_message.replace(&origin_line, &source_line)
    );
    assert_eq!(
        core_pub.tree_sha("main", None),
        mono.tree_sha("HEAD", Some("core"))
    );
}

#[test]
fn a_conflicted_import_export_is_read_the_same_by_both_binaries() {
    let Some(v1) = released_v1_bin() else {
        eprintln!("skipped: set MONOSPLICE_V1_BIN to a monosplice 1.0.0 binary to run this");
        return;
    };

    // 1.0.0 writes the public commit with the forwarded Origin; this version carries on.
    let sb = sandbox();
    let s_v1 = Script::new(v1.clone(), sb.path());
    let (mono, core_pub, ext, resolution) = conflicted_import(&s_v1);
    let v1_trailers = core_pub.git(&[
        "log",
        "-1",
        "--format=%(trailers:key=Monosplice-Origin,key=Monosplice-Source)",
        "main",
    ]);
    assert!(
        v1_trailers.contains("Monosplice-Origin") && v1_trailers.contains(&resolution),
        "{v1_trailers}"
    );
    let s = Script {
        bin: monosplice_bin(),
        root: sb.path().to_path_buf(),
        clock: Cell::new(s_v1.clock.get()),
    };
    conflicted_round(&s, &mono, &ext);

    // This version writes it without; 1.0.0 carries on.
    let sb = sandbox();
    let s_new = Script::new(monosplice_bin(), sb.path());
    let (mono, _core_pub, ext, _resolution) = conflicted_import(&s_new);
    let s = Script {
        bin: v1,
        root: sb.path().to_path_buf(),
        clock: Cell::new(s_new.clock.get()),
    };
    conflicted_round(&s, &mono, &ext);
}
