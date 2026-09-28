//! e2e: which `monosplice.toml` a command runs with (S86).
//!
//! The config is found by walking up from the cwd, but the git commands run against whatever
//! repository the cwd is in. The two must agree: a config only describes the repository whose
//! top level holds it. The nested case is a monorepo that vendors another monosplice monorepo
//! and so carries that monorepo's `monosplice.toml` as ordinary content.

mod common;

use std::path::Path;

use common::{
    clone_remote, make_bare_remote, make_repo, run_monosplice, sandbox, standard_fixture,
    subrepo_block, toml_str, write_config, Sandbox, TestRepo,
};

const CONFIG: &str = "monosplice.toml";

struct Nested {
    sandbox: Sandbox,
    /// The outer monorepo; vendors `middle` at `vendor/middle/`.
    outer: TestRepo,
    /// Bare remote `lib/` of middle is published to.
    leaf_dir: String,
    /// Bare remote of the middle monorepo itself.
    middle_dir: String,
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// middle splices `lib/` out to `leaf.git` and is itself pushed to `middle.git`; outer attaches
/// `middle.git` at `vendor/middle`, so `outer/vendor/middle/monosplice.toml` exists.
fn nested_fixture() -> Nested {
    let sb = sandbox();
    let leaf_dir = make_bare_remote(sb.path(), "leaf");
    let middle_dir = make_bare_remote(sb.path(), "middle");

    let middle = make_repo(sb.path(), "middle-src");
    write_config(
        &middle,
        &[&subrepo_block(&[
            ("name", &toml_str("lib")),
            ("path", &toml_str("lib")),
            ("remote", &toml_str(&leaf_dir)),
        ])],
    );
    middle.commit(
        "middle: initial",
        &[
            ("lib/a.txt", Some("lib v1\n")),
            ("app/main.txt", Some("app\n")),
        ],
    );
    let res = run_monosplice(&middle.dir, &["push", "lib", "--yes"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    middle.git(&["push", &middle_dir, "main"]);

    let outer = make_repo(sb.path(), "outer");
    write_config(&outer, &[]);
    outer.commit("outer: initial", &[("README.md", Some("outer\n"))]);
    let res = run_monosplice(&outer.dir, &["attach", "vendor/middle", &middle_dir]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(outer.exists("vendor/middle/monosplice.toml"));

    Nested {
        sandbox: sb,
        outer,
        leaf_dir,
        middle_dir,
    }
}

fn refs(repo: &TestRepo, prefix: &str) -> String {
    repo.git(&["for-each-ref", "--format=%(refname)", prefix])
}

fn bare_head(dir: &str) -> String {
    TestRepo::new(dir).git(&["rev-parse", "main"])
}

/// S86: every command run inside the vendored monorepo refuses instead of applying its config
/// to the outer repository, and writes nothing anywhere.
#[test]
fn s86_refuses_a_nested_config_that_is_not_at_the_git_top_level() {
    let fx = nested_fixture();
    let outer = &fx.outer;
    outer.commit(
        "outer: patch vendor/middle/lib",
        &[("vendor/middle/lib/a.txt", Some("lib v1\nouter patch\n"))],
    );
    let leaf_before = bare_head(&fx.leaf_dir);
    let middle_before = bare_head(&fx.middle_dir);
    let outer_head = outer.head();

    let nested = outer.dir.join("vendor/middle");
    let skipped = text(&nested.join(CONFIG));
    let toplevel = text(&outer.dir);
    let applies = text(&outer.dir.join(CONFIG));

    for cwd in [nested.clone(), nested.join("lib")] {
        for args in [
            &["status"][..],
            &["status", "--json"],
            &["doctor"],
            &["push"],
            &["push", "lib", "--yes"],
            &["pull"],
            &["sync"],
            &["tag", "lib", "v1.0.0"],
            &["detach", "lib"],
        ] {
            let res = run_monosplice(&cwd, args);
            assert_ne!(
                res.exit_code,
                0,
                "{args:?} in {} should refuse, stdout: {}",
                cwd.display(),
                res.stdout
            );
            for needle in [&skipped, &toplevel, &applies] {
                assert!(
                    res.stderr.contains(needle.as_str()),
                    "{args:?} must name {needle}, got:\n{}",
                    res.stderr
                );
            }
            assert!(
                res.stderr.contains("not the top of the git repository"),
                "{args:?} must say why, got:\n{}",
                res.stderr
            );
        }
    }

    assert_eq!(
        refs(outer, "refs/monosplice/lib"),
        "",
        "no refs for middle's subrepo in outer"
    );
    assert_eq!(outer.head(), outer_head, "no commit in outer");
    assert_eq!(bare_head(&fx.leaf_dir), leaf_before, "leaf untouched");
    assert_eq!(bare_head(&fx.middle_dir), middle_before, "middle untouched");
    assert!(
        outer.git(&["status", "--porcelain"]).is_empty(),
        "working tree untouched"
    );
}

/// S86: from the outer top level the nested file is just content — outer's own config runs.
#[test]
fn s86_the_outer_top_level_still_uses_its_own_config() {
    let fx = nested_fixture();
    let res = run_monosplice(&fx.outer.dir, &["status"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(res.stdout.contains("middle"), "stdout: {}", res.stdout);
    assert!(!res.stderr.contains(CONFIG), "no notice: {}", res.stderr);
}

/// S86: a nested config in a repository that has no config of its own is refused too, and the
/// message says there is none at the top level.
#[test]
fn s86_refuses_a_nested_config_when_the_top_level_has_none() {
    let sb = sandbox();
    let plain = make_repo(sb.path(), "plain");
    plain.commit(
        "plain: vendored copy of a monosplice monorepo",
        &[
            (
                "third_party/mono/monosplice.toml",
                Some("[[subrepos]]\npath = \"lib\"\nremote = \"/nowhere/leaf.git\"\n"),
            ),
            ("third_party/mono/lib/a.txt", Some("a\n")),
        ],
    );

    let cwd = plain.dir.join("third_party/mono");
    let res = run_monosplice(&cwd, &["status"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains(&text(&cwd.join(CONFIG))),
        "got:\n{}",
        res.stderr
    );
    assert!(
        res.stderr
            .contains(&format!("{} has no {CONFIG}", text(&plain.dir))),
        "got:\n{}",
        res.stderr
    );
    assert_eq!(refs(&plain, "refs/monosplice"), "");
}

/// S86: a config in a directory above the git top level does not describe that repository.
#[test]
fn s86_refuses_a_config_above_the_git_top_level() {
    let sb = sandbox();
    let parent_config = sb.path().join(CONFIG);
    std::fs::write(
        &parent_config,
        "[[subrepos]]\npath = \"core\"\nremote = \"/nowhere/core.git\"\n",
    )
    .unwrap();
    let repo = make_repo(sb.path(), "repo");
    repo.commit("init", &[("core/a.txt", Some("a\n"))]);

    for cwd in [repo.dir.clone(), repo.dir.join("core")] {
        let res = run_monosplice(&cwd, &["status"]);
        assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
        assert!(
            res.stderr.contains(&text(&parent_config)),
            "got:\n{}",
            res.stderr
        );
        assert!(
            res.stderr.contains("outside the git repository"),
            "got:\n{}",
            res.stderr
        );
        assert!(
            res.stderr.contains(&text(&repo.dir)),
            "got:\n{}",
            res.stderr
        );
    }
    assert_eq!(refs(&repo, "refs/monosplice"), "");
}

/// S86: an ordinary monorepo works from any subdirectory, exactly as before.
#[test]
fn s86_an_ordinary_monorepo_works_from_a_subdirectory() {
    let fx = standard_fixture();
    let from_root = run_monosplice(&fx.mono.dir, &["push", "core", "--yes"]);
    assert_eq!(from_root.exit_code, 0, "stderr: {}", from_root.stderr);

    fx.mono
        .commit("feat: more", &[("core/more.txt", Some("more\n"))]);
    for sub in ["core", "core/src", "private"] {
        let res = run_monosplice(&fx.mono.dir.join(sub), &["status"]);
        assert_eq!(res.exit_code, 0, "{sub}: stderr: {}", res.stderr);
        assert!(res.stdout.contains("1 to push"), "{sub}: {}", res.stdout);
    }
    let res = run_monosplice(&fx.mono.dir.join("core/src"), &["push"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert_eq!(
        TestRepo::new(&fx.pub_dir)
            .subjects("main")
            .last()
            .map(String::as_str),
        Some("feat: more")
    );
}

/// S86: a linked worktree is its own top level and carries the committed config.
#[test]
fn s86_a_linked_worktree_uses_its_own_checkout_of_the_config() {
    let fx = standard_fixture();
    let wt = fx.sandbox.path().join("wt");
    fx.mono
        .git(&["worktree", "add", "-b", "feature", &text(&wt)]);

    for cwd in [wt.clone(), wt.join("core")] {
        let res = run_monosplice(&cwd, &["status"]);
        assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
        assert!(res.stdout.contains("core"), "stdout: {}", res.stdout);
    }
}

/// S86: a worktree checked out inside the main checkout, on a branch with no config, must not
/// pick up the main checkout's config from the directory above it.
#[test]
fn s86_a_worktree_inside_the_main_checkout_without_a_config_refuses() {
    let fx = standard_fixture();
    let mono = &fx.mono;
    mono.git(&["checkout", "-q", "--orphan", "bare-branch"]);
    mono.git(&["rm", "-rq", "--cached", "."]);
    mono.git(&["clean", "-fdq"]);
    mono.commit("unrelated", &[("other.txt", Some("x\n"))]);
    mono.git(&["checkout", "-q", "-f", "main"]);

    let wt = mono.dir.join(".worktrees/bare-branch");
    mono.git(&["worktree", "add", &text(&wt), "bare-branch"]);

    let res = run_monosplice(&wt, &["status"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains(&text(&mono.dir.join(CONFIG))),
        "got:\n{}",
        res.stderr
    );
    assert!(res.stderr.contains(&text(&wt)), "got:\n{}", res.stderr);
    assert_eq!(refs(mono, "refs/monosplice"), "");
}

/// S86: a separate git repository nested inside another checkout (a submodule, or a clone
/// dropped in an ignored directory) is its own top level, so its own config applies.
#[test]
fn s86_a_nested_git_repository_uses_its_own_config() {
    let fx = nested_fixture();
    let outer = &fx.outer;
    outer.commit(
        "outer: ignore checkouts",
        &[(".gitignore", Some("checkouts/\n"))],
    );
    let clone = clone_remote(&outer.dir, &fx.middle_dir, "checkouts/middle");
    let res = run_monosplice(&clone.dir, &["status", "lib"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(res.stdout.contains("in sync"), "stdout: {}", res.stdout);
    assert_eq!(refs(outer, "refs/monosplice/lib"), "");
}

/// S86: a symlinked path to an ordinary monorepo works; a symlinked path into the vendored
/// monorepo is refused exactly like the real path.
#[cfg(unix)]
#[test]
fn s86_symlinked_paths_resolve_to_the_same_decision() {
    let fx = nested_fixture();
    let sb = fx.sandbox.path();

    let alias = sb.join("outer-alias");
    std::os::unix::fs::symlink(&fx.outer.dir, &alias).unwrap();
    let res = run_monosplice(&alias, &["status"]);
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);

    let res = run_monosplice(&alias.join("vendor/middle"), &["status"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains("not the top of the git repository"),
        "got:\n{}",
        res.stderr
    );

    let into_middle = sb.join("middle-alias");
    std::os::unix::fs::symlink(fx.outer.dir.join("vendor/middle"), &into_middle).unwrap();
    let res = run_monosplice(&into_middle, &["status"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
    assert_eq!(refs(&fx.outer, "refs/monosplice/lib"), "");
}

/// S86 / S80: a config outside any git repository still gets the not-a-repository error.
#[test]
fn s86_a_config_outside_any_git_repository_is_still_not_a_repository() {
    let sb = sandbox();
    std::fs::write(sb.path().join(CONFIG), "").unwrap();
    let res = run_monosplice(sb.path(), &["status"]);
    assert_ne!(res.exit_code, 0, "stdout: {}", res.stdout);
    assert!(
        res.stderr.contains("is not a git repository"),
        "got:\n{}",
        res.stderr
    );
}
