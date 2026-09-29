//! e2e: the CLI ergonomics batch — port of `test/e2e/ergonomics.test.ts` (S151–S166).
//!
//! Adaptations, all per `docs/rust-port.md`:
//!
//! * S151's "Nonexistent flag" is clap's own parse error now (exit 2, naming the flag).
//! * S163 was oclif's autocomplete plugin; the Rust CLI ships `completion <shell>` instead,
//!   so the scenario asserts `completion --help` and the command's place in the root help.
//! * S165 was "`monosplice.config.js` is the default". There is one config filename now:
//!   `init` writes a `monosplice.toml` whose template loads as an empty config, a TOML config
//!   loads whatever the surrounding project is (no module system to guess at), and the
//!   two-config error became the legacy-config migration error — with a `monosplice.toml`
//!   beside a legacy file winning silently.

mod common;

use std::path::Path;

use serde_json::{json, Value};

use common::{
    clone_remote, make_bare_remote, make_repo, multi_fixture, run_monosplice, sandbox,
    standard_fixture, subrepo_block, toml_str, write_config, Fixture, Sandbox, TestRepo,
};

const CONFIG: &str = "monosplice.toml";
const LEGACY_CONFIG: &str = "monosplice.config.js";
const EXT_NAME: &str = "Ext Contributor";
const EXT_EMAIL: &str = "ext@example.test";

// ---------------------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------------------

/// Run a command that must succeed, carrying its stderr into the failure message.
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

/// Parse a command's stdout as JSON, failing as an assertion rather than a panic.
fn parse_json(label: &str, stdout: &str) -> Value {
    let parsed = serde_json::from_str::<Value>(stdout);
    let problem = parsed.as_ref().err().map(ToString::to_string);
    assert!(
        parsed.is_ok(),
        "{label} must print JSON on stdout ({problem:?}), got:\n{stdout}"
    );
    parsed.unwrap_or(Value::Null)
}

/// Object keys, sorted — the shape assertion `Object.keys(x).sort()` made in the TS.
fn sorted_keys(value: &Value) -> Vec<String> {
    match value.as_object() {
        Some(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            keys
        }
        None => Vec::new(),
    }
}

/// `expect(row).toMatchObject({...})`: the named fields, and nothing said about the rest.
fn assert_matches(row: &Value, expected: &[(&str, Value)]) {
    for (key, want) in expected {
        assert_eq!(&row[*key], want, "row.{key} — whole row: {row}");
    }
}

/// `/^<line>$/m`: some line is exactly this text.
fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|l| l == line)
}

/// `/^\s*<word>\b/m`: some line begins (after its indent) with exactly this word.
fn starts_a_line(text: &str, word: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix(word)
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    })
}

struct Seeded {
    fx: Fixture,
    pub_repo: TestRepo,
    ext: TestRepo,
}

fn seeded_with_external() -> Seeded {
    let fx = standard_fixture();
    let res = run_monosplice(&fx.mono.dir, &["push", "core", "--yes"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    let ext = clone_remote(fx.sandbox.path(), &fx.pub_dir, "ext");
    let pub_repo = TestRepo::new(&fx.pub_dir);
    Seeded { fx, pub_repo, ext }
}

struct PlainRepo {
    _sandbox: Sandbox,
    mono: TestRepo,
}

/// A monorepo with a config that configures no subrepos — what `monosplice init` leaves.
fn empty_config_repo() -> PlainRepo {
    let sandbox = sandbox();
    let mono = make_repo(sandbox.path(), "mono");
    write_config(&mono, &[]);
    mono.commit(
        "chore: initial monorepo",
        &[("app/main.ts", Some("export const app = true\n"))],
    );
    PlainRepo {
        _sandbox: sandbox,
        mono,
    }
}

// ---------------------------------------------------------------------------------------
// S151: --import-history / --export-history
// ---------------------------------------------------------------------------------------

/// S151: the old spellings are unknown flags, and clap says so (exit 2).
#[test]
fn s151_is_the_only_spelling_the_old_flags_are_unknown() {
    let fx = standard_fixture();

    let cases: [&[&str]; 3] = [
        &["push", "core", "--yes", "--full-history"],
        &["attach", "core", "--full-history"],
        &["attach", "core", "--history"],
    ];
    for args in cases {
        let res = run_monosplice(&fx.mono.dir, args);
        assert_eq!(
            res.exit_code,
            2,
            "`{}` should have been rejected, stdout: {}",
            args.join(" "),
            res.stdout
        );
        let all = format!("{}\n{}", res.stdout, res.stderr);
        assert!(
            all.to_lowercase().contains("unexpected argument"),
            "`{}` must be a usage error, got:\n{all}",
            args.join(" ")
        );
        let flag = args.last().copied().unwrap_or_default();
        assert!(
            all.contains(flag),
            "the error must name {flag}, got:\n{all}"
        );
    }
}

/// S151: `push --export-history` replays the monorepo's own commits outwards.
#[test]
fn s151_push_export_history_replays_every_monorepo_commit_on_the_first_publish() {
    let fx = standard_fixture();
    let pub_repo = TestRepo::new(&fx.pub_dir);
    fx.mono
        .commit("feat: one", &[("core/one.txt", Some("1\n"))]);

    let res = run_monosplice(&fx.mono.dir, &["push", "core", "--yes", "--export-history"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert_eq!(
        pub_repo.subjects("HEAD"),
        ["chore: initial monorepo", "feat: one"]
    );
}

/// S151: `attach --import-history` replays the standalone repo's commits inwards.
#[test]
fn s151_attach_import_history_replays_every_standalone_repo_commit_inwards() {
    let sb = sandbox();
    let mono = make_repo(sb.path(), "mono");
    let pub_dir = make_bare_remote(sb.path(), "core-pub");
    write_config(
        &mono,
        &[&subrepo_block(&[
            ("name", &toml_str("core")),
            ("path", &toml_str("core")),
            ("remote", &toml_str(&pub_dir)),
        ])],
    );
    mono.commit(
        "chore: initial monorepo",
        &[("app/main.ts", Some("export const app = true\n"))],
    );

    let up = make_repo(sb.path(), "upstream");
    up.commit_as(
        "upstream: one",
        &[("a.txt", Some("a\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    up.commit_as(
        "upstream: two",
        &[("b.txt", Some("b\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    up.git(&["push", &pub_dir, "main"]);

    let res = run_monosplice(&mono.dir, &["attach", "core", "--import-history"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    let subjects = mono.subjects("HEAD");
    assert_eq!(
        subjects[subjects.len().saturating_sub(2)..],
        ["upstream: one", "upstream: two"]
    );
}

/// S151: each flag's help names the other, and the retired spelling appears nowhere.
#[test]
fn s151_names_the_other_flag_in_every_help_text_that_offers_one() {
    let fx = standard_fixture();

    let attach = run_monosplice(&fx.mono.dir, &["attach", "--help"]);
    assert_eq!(attach.exit_code, 0, "stderr: {}", attach.stderr);
    assert!(
        attach.stdout.contains("--import-history"),
        "got:\n{}",
        attach.stdout
    );
    assert!(
        attach.stdout.contains("--export-history"),
        "got:\n{}",
        attach.stdout
    );

    let push = run_monosplice(&fx.mono.dir, &["push", "--help"]);
    assert_eq!(push.exit_code, 0, "stderr: {}", push.stderr);
    assert!(
        push.stdout.contains("--export-history"),
        "got:\n{}",
        push.stdout
    );
    // "not to be confused with" — the export flag points at the import one and back.
    assert!(
        push.stdout.contains("--import-history"),
        "got:\n{}",
        push.stdout
    );
    assert!(
        !push.stdout.contains("--full-history"),
        "got:\n{}",
        push.stdout
    );
    assert!(
        !attach.stdout.contains("--full-history"),
        "got:\n{}",
        attach.stdout
    );
}

// ---------------------------------------------------------------------------------------
// S152: empty config
// ---------------------------------------------------------------------------------------

const NO_SUBREPOS: &str =
    "no subrepos configured — run `monosplice attach <folder> <git-url>` to connect one";

/// S152: nothing configured is a state to report, not silence and not a failure.
#[test]
fn s152_says_so_instead_of_printing_nothing_and_still_exits_0() {
    let repo = empty_config_repo();

    for command in ["status", "push", "pull", "sync"] {
        let res = run_monosplice(&repo.mono.dir, &[command]);
        assert_eq!(res.exit_code, 0, "{command}: {}", res.stderr);
        assert!(
            format!("{}{}", res.stdout, res.stderr).contains(NO_SUBREPOS),
            "{command} must say there is nothing configured, got:\n{}\n{}",
            res.stdout,
            res.stderr
        );
    }
}

/// S152: `--json` stays machine-readable when there is nothing to report.
#[test]
fn s152_keeps_status_json_valid_json_and_nothing_else() {
    let repo = empty_config_repo();
    let res = run_monosplice(&repo.mono.dir, &["status", "--json"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(
        !res.stdout.contains(NO_SUBREPOS),
        "the human line may not be on the JSON stdout, got:\n{}",
        res.stdout
    );
    assert_eq!(
        parse_json("status --json", &res.stdout),
        json!({"subrepos": []})
    );
}

// ---------------------------------------------------------------------------------------
// S153: status --check
// ---------------------------------------------------------------------------------------

/// S153: the exit code is the only thing `--check` changes.
#[test]
fn s153_exits_0_in_sync_1_otherwise_with_the_human_output_unchanged() {
    let seeded = seeded_with_external();
    let mono = &seeded.fx.mono;

    let clean = run_monosplice(&mono.dir, &["status"]);
    let clean_check = run_monosplice(&mono.dir, &["status", "--check"]);
    assert_eq!(clean_check.exit_code, 0, "stderr: {}", clean_check.stderr);
    assert_eq!(clean_check.stdout, clean.stdout);

    mono.commit("feat: one", &[("core/one.txt", Some("1\n"))]);
    let ahead = run_monosplice(&mono.dir, &["status"]);
    let ahead_check = run_monosplice(&mono.dir, &["status", "--check"]);
    assert_eq!(ahead_check.exit_code, 1, "stderr: {}", ahead_check.stderr);
    assert_eq!(ahead_check.stdout, ahead.stdout);

    run_ok(&mono.dir, &["push"]);
    let pushed_check = run_monosplice(&mono.dir, &["status", "--check"]);
    assert_eq!(pushed_check.exit_code, 0, "stderr: {}", pushed_check.stderr);

    seeded.ext.git(&["fetch", "origin"]);
    seeded.ext.git(&["reset", "--hard", "origin/main"]);
    seeded.ext.commit_as(
        "external: drive-by",
        &[("x.txt", Some("x\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    seeded.ext.git(&["push", "origin", "main"]);
    let behind_check = run_monosplice(&mono.dir, &["status", "--check"]);
    assert_eq!(behind_check.exit_code, 1, "stderr: {}", behind_check.stderr);
}

/// S153: "never published" is not "in sync".
#[test]
fn s153_fails_on_a_subrepo_that_was_never_published() {
    let fx = standard_fixture();
    let res = run_monosplice(&fx.mono.dir, &["status", "--check"]);
    assert_eq!(res.exit_code, 1, "stderr: {}", res.stderr);
    assert!(
        res.stdout.contains("core: not published yet"),
        "got:\n{}",
        res.stdout
    );
}

/// S153: `--check --json` keeps stdout pure JSON.
#[test]
fn s153_combines_with_json_keeping_stdout_pure_json() {
    let seeded = seeded_with_external();
    seeded
        .fx
        .mono
        .commit("feat: one", &[("core/one.txt", Some("1\n"))]);

    let res = run_monosplice(&seeded.fx.mono.dir, &["status", "--check", "--json"]);
    assert_eq!(res.exit_code, 1, "stderr: {}", res.stderr);
    parse_json("status --check --json", &res.stdout);
    assert!(res.stdout.trim().starts_with('{'), "got:\n{}", res.stdout);
    assert!(res.stdout.trim().ends_with('}'), "got:\n{}", res.stdout);
}

// ---------------------------------------------------------------------------------------
// S154: doctor --json
// ---------------------------------------------------------------------------------------

const DOCTOR_KEYS: [&str; 5] = ["monorepo", "ok", "problems", "pullInProgress", "subrepos"];
const DOCTOR_SUBREPO_KEYS: [&str; 16] = [
    "ahead",
    "behind",
    "branch",
    "forkHead",
    "lastExportedMono",
    "lastExportedPub",
    "name",
    "notes",
    "path",
    "problems",
    "pubHead",
    "pushBranch",
    "reachable",
    "remote",
    "seeded",
    "upstream",
];

/// S154: one stable object on stdout, and no human report.
#[test]
fn s154_emits_one_stable_object_on_stdout_and_no_human_report() {
    let seeded = seeded_with_external();

    let res = run_monosplice(&seeded.fx.mono.dir, &["doctor", "--json"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    for human_only in ["✓", "✗", "to push:"] {
        assert!(
            !res.stdout.contains(human_only),
            "`{human_only}` is human output, got:\n{}",
            res.stdout
        );
    }

    let parsed = parse_json("doctor --json", &res.stdout);
    assert_eq!(sorted_keys(&parsed), DOCTOR_KEYS, "got: {parsed}");
    assert_eq!(parsed["ok"], json!(true));
    assert_eq!(parsed["problems"], json!(0));
    assert_eq!(parsed["pullInProgress"], Value::Null);

    let rows = parsed["subrepos"].as_array().cloned().unwrap_or_default();
    assert_eq!(rows.len(), 1, "got: {parsed}");
    let row = &parsed["subrepos"][0];
    assert_eq!(sorted_keys(row), DOCTOR_SUBREPO_KEYS, "got: {row}");
    assert_matches(
        row,
        &[
            ("name", json!("core")),
            ("path", json!("core")),
            ("branch", json!("main")),
            ("upstream", Value::Null),
            ("reachable", json!(true)),
            ("seeded", json!(true)),
            ("pubHead", json!(seeded.pub_repo.head())),
            ("forkHead", Value::Null),
            ("ahead", json!(0)),
            ("behind", json!(0)),
            ("problems", json!([])),
        ],
    );
}

/// S154: problems change the values, never the shape or the exit contract.
#[test]
fn s154_keeps_the_same_shape_and_exit_code_when_there_are_problems() {
    let fx = standard_fixture();

    let res = run_monosplice(&fx.mono.dir, &["doctor", "--json"]);
    assert_eq!(res.exit_code, 1, "stderr: {}", res.stderr);
    let parsed = parse_json("doctor --json", &res.stdout);
    assert_eq!(sorted_keys(&parsed), DOCTOR_KEYS, "got: {parsed}");
    assert_eq!(parsed["ok"], json!(false));
    assert_eq!(parsed["problems"], json!(1));

    let row = &parsed["subrepos"][0];
    assert_eq!(sorted_keys(row), DOCTOR_SUBREPO_KEYS, "got: {row}");
    assert_matches(
        row,
        &[
            ("seeded", json!(false)),
            ("ahead", Value::Null),
            ("behind", Value::Null),
        ],
    );
    let problems = row["problems"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|p| p.as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(problems.contains("not published yet"), "got: {problems}");
}

/// S154: an unfinished pull is structured state, not just prose.
#[test]
fn s154_reports_an_unfinished_pull_as_structured_state() {
    let seeded = seeded_with_external();
    let mono = &seeded.fx.mono;

    mono.commit(
        "docs: mono wording",
        &[("core/README.md", Some("# core\n\nmono wording\n"))],
    );
    seeded.ext.commit_as(
        "docs: ext wording",
        &[("README.md", Some("# core\n\next wording\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    seeded.ext.git(&["push", "origin", "main"]);
    let pull = run_monosplice(&mono.dir, &["pull"]);
    assert_ne!(pull.exit_code, 0, "stdout: {}", pull.stdout);

    let res = run_monosplice(&mono.dir, &["doctor", "--json"]);
    assert_eq!(res.exit_code, 1, "stderr: {}", res.stderr);
    let parsed = parse_json("doctor --json", &res.stdout);
    assert_eq!(parsed["pullInProgress"]["subrepo"], json!("core"));
    assert!(
        parsed["pullInProgress"]["statePath"]
            .as_str()
            .unwrap_or_default()
            .contains("pull-state.json"),
        "got: {parsed}"
    );
}

// ---------------------------------------------------------------------------------------
// S155: uniform multi-subrepo failure policy
// ---------------------------------------------------------------------------------------

/// S155: one refusal does not stop the other subrepos from being pulled.
#[test]
fn s155_keeps_pulling_the_other_subrepos_after_one_refuses() {
    let mfx = multi_fixture();
    let mono = &mfx.mono;
    run_ok(&mono.dir, &["push", "lib", "--yes"]);

    let ext = clone_remote(mfx.sandbox.path(), &mfx.lib_pub_dir, "lib-ext");
    ext.commit_as(
        "external: lib drive-by",
        &[("drive.txt", Some("d\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    ext.git(&["push", "origin", "main"]);

    // `core` is first in the config and is not published, so it fails before `lib` is reached.
    let res = run_monosplice(&mono.dir, &["pull"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains("monosplice push core --yes"),
        "got:\n{}",
        res.stderr
    );
    assert!(
        res.stdout.contains("lib: imported 1 commit"),
        "got:\n{}",
        res.stdout
    );
    assert_eq!(
        mono.subjects("HEAD").last().map(String::as_str),
        Some("external: lib drive-by")
    );
}

/// S155: the same policy for `sync`.
#[test]
fn s155_keeps_syncing_the_other_subrepos_after_one_refuses() {
    let mfx = multi_fixture();
    let mono = &mfx.mono;
    run_ok(&mono.dir, &["push", "lib", "--yes"]);

    let ext = clone_remote(mfx.sandbox.path(), &mfx.lib_pub_dir, "lib-ext");
    ext.commit_as(
        "external: lib drive-by",
        &[("drive.txt", Some("d\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    ext.git(&["push", "origin", "main"]);
    mono.commit("feat: lib work", &[("packages/lib/new.txt", Some("n\n"))]);

    let res = run_monosplice(&mono.dir, &["sync"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains("monosplice push core --yes"),
        "got:\n{}",
        res.stderr
    );
    // Both sides moved, so the import lands on top of the local commit and both export (S43).
    assert!(
        res.stdout.contains("lib: imported 1, exported 2"),
        "got:\n{}",
        res.stdout
    );
    assert!(
        mfx.lib_pub
            .subjects("HEAD")
            .iter()
            .any(|s| s == "feat: lib work"),
        "got: {:?}",
        mfx.lib_pub.subjects("HEAD")
    );
}

/// S155: an import conflict is the exception — only one sequencer may exist, so the run stops.
#[test]
fn s155_stops_the_whole_run_on_an_import_conflict() {
    let mfx = multi_fixture();
    let mono = &mfx.mono;
    run_ok(&mono.dir, &["push", "--yes"]);

    let core_ext = clone_remote(mfx.sandbox.path(), &mfx.core_pub_dir, "core-ext");
    core_ext.commit_as(
        "external: core wording",
        &[("README.md", Some("# core\n\next wording\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    core_ext.git(&["push", "origin", "main"]);
    let lib_ext = clone_remote(mfx.sandbox.path(), &mfx.lib_pub_dir, "lib-ext");
    lib_ext.commit_as(
        "external: lib drive-by",
        &[("drive.txt", Some("d\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    lib_ext.git(&["push", "origin", "main"]);

    mono.commit(
        "docs: mono wording",
        &[("core/README.md", Some("# core\n\nmono wording\n"))],
    );

    let res = run_monosplice(&mono.dir, &["pull"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains("monosplice pull --continue"),
        "got:\n{}",
        res.stderr
    );
    assert!(
        res.stderr.contains("monosplice pull --abort"),
        "got:\n{}",
        res.stderr
    );
    // lib was never reached: its external commit is still waiting.
    assert!(!res.stdout.contains("lib:"), "got:\n{}", res.stdout);
    assert!(
        !mono
            .subjects("HEAD")
            .iter()
            .any(|s| s == "external: lib drive-by"),
        "the lib import must not have happened"
    );
    assert!(
        mfx.lib_pub
            .subjects("HEAD")
            .iter()
            .any(|s| s == "external: lib drive-by"),
        "the lib commit is still waiting on the remote"
    );
}

// ---------------------------------------------------------------------------------------
// S156: wording and streams
// ---------------------------------------------------------------------------------------

/// S156: the other repo is the "standalone" repo, never the "public" one.
#[test]
fn s156_never_calls_the_other_repo_public_in_a_command_description() {
    let repo = empty_config_repo();

    let root = run_monosplice(&repo.mono.dir, &["--help"]);
    assert_eq!(root.exit_code, 0, "stderr: {}", root.stderr);
    assert!(
        !root.stdout.to_lowercase().contains("public"),
        "got:\n{}",
        root.stdout
    );

    for command in [
        "attach", "detach", "push", "pull", "sync", "status", "doctor", "tag", "init",
    ] {
        let res = run_monosplice(&repo.mono.dir, &[command, "--help"]);
        assert_eq!(res.exit_code, 0, "{command}: {}", res.stderr);
        assert!(
            !res.stdout.to_lowercase().contains("public"),
            "{command} calls it public, got:\n{}",
            res.stdout
        );
    }
}

/// S156: the counts are the report; the `!` annotations are diagnostics, so they go to stderr.
#[test]
fn s156_sends_status_diagnostics_to_stderr_so_stdout_stays_pipeable() {
    let seeded = seeded_with_external();
    let mono = &seeded.fx.mono;

    mono.commit(
        "docs: mono wording",
        &[("core/README.md", Some("# core\n\nmono wording\n"))],
    );
    seeded.ext.commit_as(
        "docs: ext wording",
        &[("README.md", Some("# core\n\next wording\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    seeded.ext.git(&["push", "origin", "main"]);
    let pull = run_monosplice(&mono.dir, &["pull"]);
    assert_ne!(pull.exit_code, 0, "stdout: {}", pull.stdout);

    let res = run_monosplice(&mono.dir, &["status"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(!res.stdout.contains('!'), "got:\n{}", res.stdout);
    assert!(!res.stdout.contains("--continue"), "got:\n{}", res.stdout);
    assert!(res.stderr.contains("--continue"), "got:\n{}", res.stderr);
    assert!(
        res.stderr.contains("monosplice pull --abort"),
        "got:\n{}",
        res.stderr
    );

    let json = run_monosplice(&mono.dir, &["status", "--json"]);
    assert!(json.stdout.trim().starts_with('{'), "got:\n{}", json.stdout);
    parse_json("status --json", &json.stdout);
}

// ---------------------------------------------------------------------------------------
// S162: status --offline
// ---------------------------------------------------------------------------------------

const OFFLINE_NOTE: &str = "offline: using last-fetched state";

/// S162: `--offline` reports from the last fetch and never talks to the remote.
#[test]
fn s162_reports_from_the_last_fetch_and_never_talks_to_the_remote() {
    let seeded = seeded_with_external();
    let mono = &seeded.fx.mono;
    seeded.ext.commit_as(
        "external: drive-by",
        &[("x.txt", Some("x\n"))],
        EXT_NAME,
        EXT_EMAIL,
    );
    seeded.ext.git(&["push", "origin", "main"]);

    // Move the remote out of the way: anything that fetches now fails.
    let moved = format!("{}-moved", seeded.pub_repo.dir.display());
    std::fs::rename(&seeded.pub_repo.dir, &moved).expect("move the remote aside");

    let offline = run_monosplice(&mono.dir, &["status", "--offline"]);
    let online_while_moved = run_monosplice(&mono.dir, &["status"]);
    std::fs::rename(&moved, &seeded.pub_repo.dir).expect("put the remote back");

    assert_eq!(offline.exit_code, 0, "stderr: {}", offline.stderr);
    assert!(
        has_line(&offline.stdout, "core: in sync"),
        "got:\n{}",
        offline.stdout
    );
    assert!(
        offline.stderr.contains(OFFLINE_NOTE),
        "got:\n{}",
        offline.stderr
    );
    assert_ne!(
        online_while_moved.exit_code, 0,
        "stdout: {}",
        online_while_moved.stdout
    );

    // With the remote back, the online run sees what --offline could not.
    let seen = run_monosplice(&mono.dir, &["status"]);
    assert_eq!(seen.exit_code, 0, "stderr: {}", seen.stderr);
    assert!(
        has_line(&seen.stdout, "core: 1 to pull"),
        "got:\n{}",
        seen.stdout
    );
}

/// S162: the note is a property of the run, not of each subrepo.
#[test]
fn s162_says_so_once_per_run_not_once_per_subrepo() {
    let mfx = multi_fixture();
    run_ok(&mfx.mono.dir, &["push", "--yes"]);

    let res = run_monosplice(&mfx.mono.dir, &["status", "--offline"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert_eq!(
        res.stderr.matches(OFFLINE_NOTE).count(),
        1,
        "got:\n{}",
        res.stderr
    );
}

/// S162: never fetched is reported as such, not guessed at.
#[test]
fn s162_refuses_to_guess_for_a_subrepo_that_was_never_fetched() {
    let fx = standard_fixture();
    let res = run_monosplice(&fx.mono.dir, &["status", "--offline"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(
        res.stdout
            .contains("core: no fetch yet — run without --offline first"),
        "got:\n{}",
        res.stdout
    );
    assert!(!res.stdout.contains("in sync"), "got:\n{}", res.stdout);

    let check = run_monosplice(&fx.mono.dir, &["status", "--offline", "--check"]);
    assert_eq!(check.exit_code, 1, "stderr: {}", check.stderr);
}

/// S162: `--offline --json` adds a top-level flag and leaves the row key set alone.
#[test]
fn s162_combines_with_json_without_changing_the_row_key_set() {
    let seeded = seeded_with_external();
    let mono = &seeded.fx.mono;

    let online = run_monosplice(&mono.dir, &["status", "--json"]);
    assert_eq!(online.exit_code, 0, "stderr: {}", online.stderr);
    let offline = run_monosplice(&mono.dir, &["status", "--offline", "--json"]);
    assert_eq!(offline.exit_code, 0, "stderr: {}", offline.stderr);

    let parsed = parse_json("status --offline --json", &offline.stdout);
    assert_eq!(parsed["offline"], json!(true));
    let online_parsed = parse_json("status --json", &online.stdout);
    assert_eq!(
        sorted_keys(&parsed["subrepos"][0]),
        sorted_keys(&online_parsed["subrepos"][0])
    );
}

// ---------------------------------------------------------------------------------------
// S163: shell completion (clap_complete replaces oclif autocomplete)
// ---------------------------------------------------------------------------------------

/// S163: the CLI ships a completion command, and the root help says so.
#[test]
fn s163_ships_shell_completion() {
    let repo = empty_config_repo();
    let help = run_monosplice(&repo.mono.dir, &["completion", "--help"]);
    assert_eq!(help.exit_code, 0, "stderr: {}", help.stderr);

    let root = run_monosplice(&repo.mono.dir, &["--help"]);
    assert_eq!(root.exit_code, 0, "stderr: {}", root.stderr);
    assert!(
        starts_a_line(&root.stdout, "completion"),
        "got:\n{}",
        root.stdout
    );
}

// ---------------------------------------------------------------------------------------
// S165: monosplice.toml is the one config file
// ---------------------------------------------------------------------------------------

/// S165: `init` writes a `monosplice.toml` that loads as an empty config.
#[test]
fn s165_init_writes_a_monosplice_toml_that_loads_as_an_empty_config() {
    let sb = sandbox();
    let mono = make_repo(sb.path(), "mono");

    let res = run_monosplice(&mono.dir, &["init"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(mono.exists(CONFIG));
    assert!(!mono.exists("monosplice.config.js"));
    assert!(!mono.exists("monosplice.config.ts"));
    assert!(res.stdout.contains(CONFIG), "got:\n{}", res.stdout);

    let written = mono.read(CONFIG);
    // Every meaningful line but the monorepo's `id` (S184) is commented out, which is what
    // "loads as an empty config" looks like from outside: nothing is attached until `attach`
    // appends the first entry.
    for line in written.lines() {
        let trimmed = line.trim();
        assert!(
            trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("id = \""),
            "the scaffold configures nothing yet, but this line does: {line}"
        );
    }
    let status = run_monosplice(&mono.dir, &["status"]);
    assert_eq!(status.exit_code, 0, "stderr: {}", status.stderr);
    assert!(
        written.contains("[[subrepos]]"),
        "the scaffold must show the entry shape, got:\n{written}"
    );
    assert!(
        written.contains("path") && written.contains("remote"),
        "got:\n{written}"
    );
    assert!(written.contains("monosplice"), "got:\n{written}");
}

/// S165: a TOML config is a first-class citizen whatever the surrounding project is — there
/// is no module system left to guess at (the reason the JS scaffold needed a `@type` hint).
#[test]
fn s165_loads_a_toml_config_whatever_the_surrounding_project_is() {
    for (label, pkg) in [
        ("no package.json", None),
        (
            "a CommonJS package.json",
            Some("{\"name\": \"their-monorepo\"}\n"),
        ),
        (
            "an ESM package.json",
            Some("{\"name\": \"their-monorepo\", \"type\": \"module\"}\n"),
        ),
    ] {
        let sb = sandbox();
        let mono = make_repo(sb.path(), "mono");
        let pub_dir = make_bare_remote(sb.path(), "core-pub");
        write_config(
            &mono,
            &[&subrepo_block(&[
                ("name", &toml_str("core")),
                ("path", &toml_str("core")),
                ("remote", &toml_str(&pub_dir)),
            ])],
        );
        if let Some(pkg) = pkg {
            mono.write("package.json", pkg);
        }
        mono.commit(
            "chore: initial monorepo",
            &[("core/README.md", Some("# core\n"))],
        );

        let res = run_monosplice(&mono.dir, &["push", "core", "--yes"]);
        assert_eq!(res.exit_code, 0, "{label}: {}", res.stderr);
        let status = run_monosplice(&mono.dir, &["status"]);
        assert!(
            has_line(&status.stdout, "core: in sync"),
            "{label}, got:\n{}",
            status.stdout
        );
    }
}

/// S165: a JavaScript-era config with no `monosplice.toml` beside it stops every command,
/// naming the file it found, the file monosplice reads now, and where the migration is
/// written down.
#[test]
fn s165_a_legacy_js_config_with_no_toml_stops_every_command_naming_both() {
    let sb = sandbox();
    let mono = make_repo(sb.path(), "mono");
    mono.write(LEGACY_CONFIG, "export default {subrepos: []}\n");
    mono.commit(
        "chore: initial monorepo",
        &[("app/main.ts", Some("export const app = true\n"))],
    );

    let commands: [&[&str]; 7] = [
        &["status"],
        &["push"],
        &["pull"],
        &["sync"],
        &["doctor"],
        &["init"],
        &["attach", "core"],
    ];
    for command in commands {
        let res = run_monosplice(&mono.dir, command);
        assert_ne!(
            res.exit_code,
            0,
            "`{}` should have failed, stdout: {}",
            command.join(" "),
            res.stdout
        );
        let out = format!("{}\n{}", res.stdout, res.stderr);
        assert!(
            out.contains(LEGACY_CONFIG),
            "`{}` must name the legacy file, got:\n{out}",
            command.join(" ")
        );
        assert!(
            out.contains(CONFIG),
            "`{}` must name the config monosplice reads now, got:\n{out}",
            command.join(" ")
        );
        assert!(
            out.contains("docs/reference.md"),
            "`{}` must point at the migration guide, got:\n{out}",
            command.join(" ")
        );
    }
}

/// S165: a repo mid-migration has both files, and the TOML wins without a word about it.
#[test]
fn s165_a_monosplice_toml_beside_a_legacy_config_wins_silently() {
    let sb = sandbox();
    let mono = make_repo(sb.path(), "mono");
    mono.write(LEGACY_CONFIG, "export default {subrepos: []}\n");
    write_config(&mono, &[]);
    mono.commit(
        "chore: initial monorepo",
        &[("app/main.ts", Some("export const app = true\n"))],
    );

    let res = run_monosplice(&mono.dir, &["init"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(
        res.stdout.contains("Already initialized"),
        "got:\n{}",
        res.stdout
    );
    assert!(res.stdout.contains(CONFIG), "got:\n{}", res.stdout);
    assert!(
        !res.stdout.contains(LEGACY_CONFIG),
        "the legacy file is presumed mid-migration and stays unmentioned, got:\n{}",
        res.stdout
    );
}

// ---------------------------------------------------------------------------------------
// S166: leading ./ on a user-supplied path
// ---------------------------------------------------------------------------------------

/// S166: `attach ./core` is `attach core`, and the config records the normalized path.
#[test]
fn s166_attaches_dot_slash_core_exactly_like_core() {
    let sb = sandbox();
    let mono = make_repo(sb.path(), "mono");
    let pub_dir = make_bare_remote(sb.path(), "core-pub");
    write_config(&mono, &[]);
    mono.commit(
        "chore: initial monorepo",
        &[("core/README.md", Some("# core\n"))],
    );

    let res = run_monosplice(&mono.dir, &["attach", "./core", &pub_dir, "--yes"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(
        mono.read(CONFIG).contains("path = \"core\""),
        "got:\n{}",
        mono.read(CONFIG)
    );
    assert!(
        !mono.read(CONFIG).contains("path = \"./core\""),
        "got:\n{}",
        mono.read(CONFIG)
    );
    let status = run_monosplice(&mono.dir, &["status"]);
    assert!(
        has_line(&status.stdout, "core: in sync"),
        "got:\n{}",
        status.stdout
    );
}

/// S166: only the leading `./` is forgiven; `.` and `..` segments stay rejected.
#[test]
fn s166_still_rejects_dot_and_dot_dot_segments() {
    let repo = empty_config_repo();
    for folder in [".", "./", "./..", "a/../b"] {
        let res = run_monosplice(
            &repo.mono.dir,
            &["attach", folder, "git@example.test:x/y.git"],
        );
        assert_ne!(res.exit_code, 0, "{folder}, stdout: {}", res.stdout);
    }
}
