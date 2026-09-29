//! e2e: `monosplice init` — port of `test/e2e/init.test.ts`.
//!
//! Adapted per `docs/rust-port.md`: the scaffolded config is `monosplice.toml`, a commented
//! `[[subrepos]]` template rather than a JS `export default`.

mod common;

use common::{
    clone_remote, deny_pushes, make_bare_remote, make_repo, run_monosplice, sandbox, TestRepo,
};

/// True when some line of `text` is a comment that mentions `needle`.
fn has_commented(text: &str, needle: &str) -> bool {
    text.lines()
        .any(|l| l.trim_start().starts_with('#') && l.contains(needle))
}

/// S01: `init` scaffolds `monosplice.toml`; running it again is a safe no-op.
#[test]
fn s01_init_scaffolds_monosplice_toml_and_rerun_is_a_safe_no_op() {
    let sb = sandbox();
    let mono = make_repo(sb.path(), "mono");

    let first = run_monosplice(&mono.dir, &["init"]);
    assert_eq!(first.exit_code, 0, "stderr: {}", first.stderr);
    assert!(
        mono.exists("monosplice.toml"),
        "init must create monosplice.toml"
    );

    let written = mono.read("monosplice.toml");
    assert!(
        has_commented(&written, "[[subrepos]]"),
        "scaffold must carry a commented [[subrepos]] example, got:\n{written}"
    );
    assert!(
        written.contains("path") && written.contains("remote"),
        "scaffold must show the path/remote keys, got:\n{written}"
    );

    assert!(
        first.stdout.contains("monosplice attach"),
        "init should point at `monosplice attach`, got:\n{}",
        first.stdout
    );

    let before = mono.read("monosplice.toml");
    let second = run_monosplice(&mono.dir, &["init"]);
    assert_eq!(second.exit_code, 0, "stderr: {}", second.stderr);
    assert!(
        second.stdout.to_lowercase().contains("already initialized"),
        "re-running init should report it is already initialized, got:\n{}",
        second.stdout
    );
    assert_eq!(
        mono.read("monosplice.toml"),
        before,
        "re-running init must not rewrite the config"
    );
}

/// S01: `init` refuses to run outside a git repository (and names `git init`).
#[test]
fn s01_init_refuses_outside_a_git_repository() {
    let sb = sandbox();
    let dir = sb.path().join("not-a-repo");
    std::fs::create_dir_all(&dir).expect("create not-a-repo");

    let res = run_monosplice(&dir, &["init"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);

    let stderr = res.stderr.to_lowercase();
    assert!(
        stderr.contains("git repository"),
        "error must say we are not in a git repository, got:\n{}",
        res.stderr
    );
    assert!(
        stderr.contains("git init"),
        "error must name `git init` as the fix, got:\n{}",
        res.stderr
    );
    assert!(
        !dir.join("monosplice.toml").exists(),
        "no config may be written outside a repo"
    );
}

/// The harness itself, without the CLI: if this fails, every other e2e result is noise.
#[test]
fn harness_selftest() {
    let sb = sandbox();
    let repo = make_repo(sb.path(), "repo");

    let c1 = repo.commit(
        "feat: one",
        &[("a.txt", Some("a\n")), ("dir/b.txt", Some("b\n"))],
    );
    let c2 = repo.commit_as(
        "fix: two\n\nA body paragraph.\n\nTrailer-Key: value",
        &[("a.txt", Some("a2\n")), ("dir/b.txt", None)],
        "Ada Lovelace",
        "ada@example.test",
    );

    assert_eq!(c1.len(), 40, "rev-parse should hand back a full sha: {c1}");
    assert_ne!(c1, c2);
    assert_eq!(repo.head(), c2);

    // subjects / messages / authors
    assert_eq!(repo.subjects("HEAD"), vec!["feat: one", "fix: two"]);

    let messages = repo.messages("HEAD");
    assert_eq!(messages.len(), 2, "messages: {messages:?}");
    assert_eq!(messages[0], "feat: one");
    assert!(messages[1].starts_with("fix: two"));
    assert!(messages[1].contains("A body paragraph."));
    assert!(messages[1].ends_with("Trailer-Key: value"));

    assert_eq!(
        repo.authors("HEAD"),
        vec![
            "Mono Author <mono@example.test>",
            "Ada Lovelace <ada@example.test>"
        ]
    );

    // working tree helpers
    assert!(repo.exists("a.txt"));
    assert!(
        !repo.exists("dir/b.txt"),
        "None content must delete the file"
    );
    assert_eq!(repo.read("a.txt"), "a2\n");

    // file_at: one trailing newline is stripped, exactly as in the TS harness
    assert_eq!(repo.file_at(&c1, "a.txt"), "a");
    assert_eq!(repo.file_at("HEAD", "a.txt"), "a2");

    // tree_entries / tree_sha
    // Entries sort by the whole `mode sha path` line (i.e. by sha), as in the TS harness.
    let entries = repo.tree_entries(&c1, None);
    let mut paths: Vec<&str> = entries
        .iter()
        .map(|e| e.split(' ').nth(2).expect("mode sha path"))
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["a.txt", "dir/b.txt"]);
    assert_eq!(entries, repo.tree_entries("HEAD~1", None));
    assert!(
        entries.iter().all(|e| e.starts_with("100644 ")),
        "entries: {entries:?}"
    );

    let sub_entries = repo.tree_entries(&c1, Some("dir"));
    assert_eq!(sub_entries.len(), 1, "sub_entries: {sub_entries:?}");
    assert!(sub_entries[0].ends_with(" b.txt"));

    let t1 = repo.tree_sha(&c1, None);
    let t2 = repo.tree_sha(&c2, None);
    assert_eq!(t1.len(), 40);
    assert_ne!(t1, t2);
    assert_eq!(
        repo.tree_sha(&c1, Some("dir")),
        repo.git(&["rev-parse", &format!("{c1}:dir")]),
        "tree_sha(subpath) must resolve <treeish>:<subpath>"
    );

    // bare remote round-trip
    let remote = make_bare_remote(sb.path(), "origin");
    repo.git(&["remote", "add", "origin", &remote]);
    repo.git(&["push", "origin", "main"]);

    let clone = clone_remote(sb.path(), &remote, "clone");
    assert_eq!(clone.head(), c2);
    assert_eq!(clone.subjects("HEAD"), vec!["feat: one", "fix: two"]);
    assert_eq!(clone.read("a.txt"), "a2\n");

    // a TestRepo view of the bare repo itself works for read-only plumbing
    let bare = TestRepo::new(&remote);
    assert_eq!(bare.git(&["rev-parse", "main"]), c2);

    // deny_pushes: readable, not writable
    let denied = make_bare_remote(sb.path(), "denied");
    deny_pushes(&denied);

    let ls = repo.git_try(&["ls-remote", &denied]);
    assert_eq!(
        ls.exit_code, 0,
        "ls-remote must keep working on a deny_pushes remote: {}",
        ls.stderr
    );

    let push = repo.git_try(&["push", &denied, "main"]);
    assert_ne!(
        push.exit_code, 0,
        "push to a deny_pushes remote must fail (stdout: {})",
        push.stdout
    );
}

/// The `id = "…"` line of a config, if any.
fn config_id(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let rest = l.trim().strip_prefix("id")?.trim_start().strip_prefix('=')?;
        Some(rest.trim().trim_matches('"').to_string())
    })
}

/// S184: `init` gives every new monorepo its own `id`, and an export from it carries that id
/// after its `Monosplice-Source` — the line another monorepo publishing the same standalone
/// branch reads to tell the two apart.
#[test]
fn s184_init_writes_a_fresh_id_that_every_export_carries() {
    let sb = sandbox();
    let one = make_repo(sb.path(), "one");
    let two = make_repo(sb.path(), "two");
    assert_eq!(run_monosplice(&one.dir, &["init"]).exit_code, 0);
    assert_eq!(run_monosplice(&two.dir, &["init"]).exit_code, 0);
    let id_one = config_id(&one.read("monosplice.toml")).expect("init writes an id");
    let id_two = config_id(&two.read("monosplice.toml")).expect("init writes an id");
    assert_ne!(id_one, id_two, "every monorepo gets its own id");
    assert!(id_one.len() >= 16, "{id_one}");
    assert!(
        id_one
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')),
        "{id_one}"
    );

    let pub_dir = make_bare_remote(sb.path(), "lib");
    one.commit("lib v1", &[("lib/a.txt", Some("v1\n"))]);
    let attach = run_monosplice(&one.dir, &["attach", "lib", &pub_dir, "--yes"]);
    assert_eq!(attach.exit_code, 0, "{}", attach.stderr);
    one.commit("lib v2", &[("lib/a.txt", Some("v2\n"))]);
    assert_eq!(run_monosplice(&one.dir, &["push"]).exit_code, 0);
    let lib = TestRepo::new(&pub_dir);
    for rev in ["main", "main~1"] {
        let trailers = lib.git(&[
            "log",
            "-1",
            "--format=%(trailers:key=Monosplice-Source,key=Monosplice-Monorepo)",
            rev,
        ]);
        let lines: Vec<&str> = trailers.lines().collect();
        assert_eq!(lines.len(), 2, "{rev}: {trailers}");
        assert!(lines[0].starts_with("Monosplice-Source: "), "{trailers}");
        assert_eq!(lines[1], format!("Monosplice-Monorepo: {id_one}"));
    }
}

/// S184: an `id` is a single token; anything that could not sit on one trailer line is refused
/// with the file and the field named.
#[test]
fn s184_an_id_that_cannot_be_a_trailer_value_is_refused() {
    let sb = sandbox();
    let mono = make_repo(sb.path(), "mono");
    mono.write("monosplice.toml", "id = \"two words\"\n");
    let res = run_monosplice(&mono.dir, &["status"]);
    assert_ne!(res.exit_code, 0, "{}", res.stdout);
    assert!(res.stderr.contains("monosplice.toml"), "{}", res.stderr);
    assert!(res.stderr.contains("id"), "{}", res.stderr);
}
