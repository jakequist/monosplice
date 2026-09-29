//! e2e harness — Rust port of `test/e2e/harness.ts`.
//!
//! Black-box only: tests run the built binary via `CARGO_BIN_EXE_monosplice` and assert on
//! exit codes, stdout/stderr and resulting git state. Nothing here touches crate internals.
//!
//! Helpers land here ahead of the test files that need them, so unused-code warnings are
//! expected until the rest of the suite is ported.
#![allow(dead_code)]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Deterministic git environment: no user/system config, fixed identities.
/// Dates are assigned per-commit by [`next_date`] so consecutive commits with
/// identical content still get distinct shas.
pub fn git_env() -> &'static [(&'static str, &'static str)] {
    &[
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_AUTHOR_NAME", "Mono Author"),
        ("GIT_AUTHOR_EMAIL", "mono@example.test"),
        ("GIT_COMMITTER_NAME", "Mono Committer"),
        ("GIT_COMMITTER_EMAIL", "committer@example.test"),
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "commit.gpgsign"),
        ("GIT_CONFIG_VALUE_0", "false"),
        ("GIT_CONFIG_KEY_1", "init.defaultBranch"),
        ("GIT_CONFIG_VALUE_1", "main"),
        ("NO_COLOR", "1"),
    ]
}

static DATE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Monotonic fake timestamps (base 2026-01-01, +61s per commit).
///
/// The counter is process-global — cargo runs tests in threads, so uniqueness has to hold
/// across the whole binary, not per test.
pub fn next_date() -> String {
    let n = DATE_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    format!("{} +0000", 1_767_225_600 + n * 61)
}

static SANDBOX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Temp directory removed when the guard is dropped.
///
/// Tests must bind it to a local (`let sb = sandbox();`) and keep it alive for the whole
/// test — dropping it early deletes the repos underneath.
pub struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub fn sandbox() -> Sandbox {
    let n = SANDBOX_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("monosplice-e2e-{}-{}", std::process::id(), n));
    fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("failed to create sandbox {}: {e}", dir.display()));
    Sandbox { dir }
}

pub struct RunResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Run the built monosplice CLI (black-box) in a directory. Never panics on non-zero exit.
pub fn run_monosplice(cwd: &Path, args: &[&str]) -> RunResult {
    run_monosplice_env(cwd, args, &[])
}

/// [`run_monosplice`] with extra environment on top of [`git_env`] (later wins).
pub fn run_monosplice_env(cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> RunResult {
    run_binary_env(
        Path::new(env!("CARGO_BIN_EXE_monosplice")),
        cwd,
        args,
        extra_env,
    )
}

/// The built binary under test.
pub fn monosplice_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_monosplice"))
}

/// A released monosplice binary to cross-check against, from `MONOSPLICE_V1_BIN`. Tests that
/// need one skip (and say so) when it is unset; CI does not install it.
pub fn released_v1_bin() -> Option<PathBuf> {
    let path = std::env::var_os("MONOSPLICE_V1_BIN")?;
    let path = PathBuf::from(path);
    assert!(
        path.is_file(),
        "MONOSPLICE_V1_BIN={} is not a file",
        path.display()
    );
    Some(path)
}

/// [`run_monosplice_env`] for any monosplice binary — the one under test or a released one.
pub fn run_binary_env(
    bin: &Path,
    cwd: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> RunResult {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in git_env() {
        cmd.env(k, v);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to run monosplice {args:?}: {e}"));
    RunResult {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        // No exit code means the child was killed by a signal.
        exit_code: out.status.code().unwrap_or(-1),
    }
}

fn strip_final_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

fn spawn_git(
    cwd: &Path,
    args: &[&str],
    extra_env: &[(&str, &str)],
    input: Option<&str>,
) -> RunResult {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    for (k, v) in git_env() {
        cmd.env(k, v);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn `git {}`: {e}", args.join(" ")));
    if let Some(text) = input {
        let mut stdin = child.stdin.take().expect("git stdin was piped");
        stdin
            .write_all(text.as_bytes())
            .unwrap_or_else(|e| panic!("failed writing stdin to `git {}`: {e}", args.join(" ")));
    }
    let out = child
        .wait_with_output()
        .unwrap_or_else(|e| panic!("failed waiting for `git {}`: {e}", args.join(" ")));
    RunResult {
        stdout: strip_final_newline(String::from_utf8_lossy(&out.stdout).into_owned()),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        exit_code: out.status.code().unwrap_or(-1),
    }
}

pub struct TestRepo {
    pub dir: PathBuf,
}

impl TestRepo {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        TestRepo { dir: dir.into() }
    }

    /// Run git in this repo and return trimmed stdout. Panics loudly (command + stderr) when
    /// git fails — setup breakage should never masquerade as a test assertion.
    pub fn git(&self, args: &[&str]) -> String {
        self.git_with(args, &[], None)
    }

    /// [`TestRepo::git`] with extra environment and optional stdin.
    pub fn git_with(
        &self,
        args: &[&str],
        extra_env: &[(&str, &str)],
        input: Option<&str>,
    ) -> String {
        let res = spawn_git(&self.dir, args, extra_env, input);
        assert!(
            res.exit_code == 0,
            "`git {}` failed in {} (exit {})\n{}",
            args.join(" "),
            self.dir.display(),
            res.exit_code,
            res.stderr
        );
        res.stdout
    }

    /// Non-panicking variant, for asserting that a git command *fails*
    /// (the stand-in for the TS harness's `await expect(repo.git(...)).rejects.toThrow()`).
    pub fn git_try(&self, args: &[&str]) -> RunResult {
        spawn_git(&self.dir, args, &[], None)
    }

    pub fn write(&self, rel: &str, content: &str) {
        self.write_bytes(rel, content.as_bytes());
    }

    pub fn write_bytes(&self, rel: &str, content: &[u8]) {
        let abs = self.dir.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("failed to create {}: {e}", parent.display()));
        }
        fs::write(&abs, content)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", abs.display()));
    }

    /// Remove a file or directory; missing paths are fine (`rmSync` with `force: true`).
    pub fn rm(&self, rel: &str) {
        let abs = self.dir.join(rel);
        if abs.is_dir() {
            let _ = fs::remove_dir_all(&abs);
        } else {
            let _ = fs::remove_file(&abs);
        }
    }

    pub fn read(&self, rel: &str) -> String {
        let abs = self.dir.join(rel);
        fs::read_to_string(&abs).unwrap_or_else(|e| panic!("failed to read {}: {e}", abs.display()))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.dir.join(rel).exists()
    }

    /// Write files (`None` content = delete) and commit them all. Returns the new commit sha.
    pub fn commit(&self, message: &str, files: &[(&str, Option<&str>)]) -> String {
        self.commit_inner(message, files, None)
    }

    /// [`TestRepo::commit`] under a different author identity.
    pub fn commit_as(
        &self,
        message: &str,
        files: &[(&str, Option<&str>)],
        author_name: &str,
        author_email: &str,
    ) -> String {
        self.commit_inner(message, files, Some((author_name, author_email)))
    }

    fn commit_inner(
        &self,
        message: &str,
        files: &[(&str, Option<&str>)],
        author: Option<(&str, &str)>,
    ) -> String {
        for (rel, content) in files {
            match content {
                None => self.rm(rel),
                Some(text) => self.write(rel, text),
            }
        }
        self.git(&["add", "-A"]);
        let date = next_date();
        let mut env: Vec<(&str, &str)> = vec![
            ("GIT_AUTHOR_DATE", date.as_str()),
            ("GIT_COMMITTER_DATE", date.as_str()),
        ];
        if let Some((name, email)) = author {
            env.push(("GIT_AUTHOR_NAME", name));
            env.push(("GIT_AUTHOR_EMAIL", email));
        }
        self.git_with(&["commit", "--allow-empty", "-m", message], &env, None);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Subjects, oldest first.
    pub fn subjects(&self, r: &str) -> Vec<String> {
        let out = self.git(&["log", "--reverse", "--format=%s", r]);
        if out.is_empty() {
            Vec::new()
        } else {
            out.split('\n').map(str::to_owned).collect()
        }
    }

    /// Full raw messages, oldest first.
    ///
    /// `%B` alone is ambiguous once a message has blank lines, so each entry is terminated by
    /// a record separator (`%x1e`) and split on that instead of on newlines.
    pub fn messages(&self, r: &str) -> Vec<String> {
        let out = self.git(&["log", "--reverse", "--format=%B%x1e", r]);
        out.split('\u{1e}')
            .map(|m| m.strip_prefix('\n').unwrap_or(m).trim_end().to_owned())
            .filter(|m| !m.is_empty())
            .collect()
    }

    /// `<authorName> <authorEmail>` oldest first.
    pub fn authors(&self, r: &str) -> Vec<String> {
        let out = self.git(&["log", "--reverse", "--format=%an <%ae>", r]);
        if out.is_empty() {
            Vec::new()
        } else {
            out.split('\n').map(str::to_owned).collect()
        }
    }

    /// Sorted `mode sha path` listing of a tree-ish (optionally a subpath).
    pub fn tree_entries(&self, treeish: &str, subpath: Option<&str>) -> Vec<String> {
        let target = match subpath {
            Some(sub) => format!("{treeish}:{sub}"),
            None => treeish.to_owned(),
        };
        let out = self.git(&[
            "ls-tree",
            "-r",
            "--format=%(objectmode) %(objectname) %(path)",
            &target,
        ]);
        if out.is_empty() {
            return Vec::new();
        }
        let mut lines: Vec<String> = out.split('\n').map(str::to_owned).collect();
        lines.sort();
        lines
    }

    /// Tree object sha for a treeish (optionally a subdirectory of it).
    pub fn tree_sha(&self, treeish: &str, subpath: Option<&str>) -> String {
        let target = match subpath {
            Some(sub) => format!("{treeish}:{sub}"),
            None => format!("{treeish}^{{tree}}"),
        };
        self.git(&["rev-parse", &target])
    }

    /// Blob content at a revision (one trailing newline stripped, as in the TS harness).
    pub fn file_at(&self, treeish: &str, rel: &str) -> String {
        self.git(&["show", &format!("{treeish}:{rel}")])
    }
}

/// Init a normal (non-bare) repo with main branch.
pub fn make_repo(root: &Path, name: &str) -> TestRepo {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("failed to create {}: {e}", dir.display()));
    let repo = TestRepo::new(dir);
    repo.git(&["init", "-b", "main"]);
    repo
}

/// Bare repo usable as a local "public remote". Returns its path (valid as a git URL).
pub fn make_bare_remote(root: &Path, name: &str) -> String {
    let dir = root.join(format!("{name}.git"));
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("failed to create {}: {e}", dir.display()));
    let path = dir.to_string_lossy().into_owned();
    let res = spawn_git(root, &["init", "--bare", "-b", "main", &path], &[], None);
    assert!(
        res.exit_code == 0,
        "`git init --bare` failed for {path}\n{}",
        res.stderr
    );
    path
}

/// Make a bare repo readable but not writable, the way a public repo you have no push rights
/// to behaves. `ls-remote`/`fetch` (upload-pack) keep working; `push` — including
/// `push --dry-run` — dies before it can talk to the remote, with git's own "Could not read
/// from remote repository. Please make sure you have the correct access rights" wording.
///
/// A filesystem `chmod` cannot express this (an up-to-date local push short-circuits without
/// ever writing), and hooks never run on a dry run; poisoning a config key only `receive-pack`
/// parses is the one mechanism that is deterministic for a `file:`-style remote.
pub fn deny_pushes(bare_dir: &str) {
    let config = Path::new(bare_dir).join("config");
    let config = config.to_string_lossy().into_owned();
    let res = spawn_git(
        Path::new(bare_dir),
        &[
            "config",
            "--file",
            &config,
            "receive.maxInputSize",
            "not-a-number",
        ],
        &[],
        None,
    );
    assert!(
        res.exit_code == 0,
        "failed to poison receive.maxInputSize in {bare_dir}\n{}",
        res.stderr
    );
}

/// Clone a remote (e.g. to act as an external contributor) and return a TestRepo.
pub fn clone_remote(root: &Path, remote_dir: &str, name: &str) -> TestRepo {
    let dir = root.join(name);
    let dir_str = dir.to_string_lossy().into_owned();
    let res = spawn_git(root, &["clone", remote_dir, &dir_str], &[], None);
    assert!(
        res.exit_code == 0,
        "`git clone {remote_dir}` failed\n{}",
        res.stderr
    );
    TestRepo::new(dir)
}

/// Render a TOML basic string (quoted + escaped).
pub fn toml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Build a `[[subrepos]]` block. Values are already-rendered TOML, so callers pass
/// `toml_str(path)` for strings and array literals verbatim.
pub fn subrepo_block(fields: &[(&str, &str)]) -> String {
    let mut out = String::from("[[subrepos]]\n");
    for (key, value) in fields {
        out.push_str(key);
        out.push_str(" = ");
        out.push_str(value);
        out.push('\n');
    }
    out
}

/// Write `monosplice.toml`. Entries are full `[[subrepos]]` block strings, separated by a
/// blank line.
pub fn write_config(repo: &TestRepo, entries: &[&str]) {
    let body = entries
        .iter()
        .map(|e| e.trim_end())
        .collect::<Vec<_>>()
        .join("\n\n");
    repo.write("monosplice.toml", &format!("{body}\n"));
}

pub struct Fixture {
    pub sandbox: Sandbox,
    pub mono: TestRepo,
    pub pub_dir: String,
}

/// Standard fixture: a monorepo with a `core/` subrepo dir, private dirs, and a
/// bare public remote wired into the config.
pub fn standard_fixture() -> Fixture {
    standard_fixture_extra("")
}

/// [`standard_fixture`] with extra TOML lines appended inside the `core` `[[subrepos]]` block
/// (e.g. `exclude = ["**/*.secret"]`, or a hook key).
pub fn standard_fixture_extra(extra_lines: &str) -> Fixture {
    let sandbox = sandbox();
    let mono = make_repo(sandbox.path(), "mono");
    let pub_dir = make_bare_remote(sandbox.path(), "core-pub");

    let mut block = subrepo_block(&[
        ("name", &toml_str("core")),
        ("path", &toml_str("core")),
        ("remote", &toml_str(&pub_dir)),
    ]);
    let extra = extra_lines.trim();
    if !extra.is_empty() {
        block.push_str(extra);
        block.push('\n');
    }
    write_config(&mono, &[&block]);

    mono.commit(
        "chore: initial monorepo",
        &[
            ("core/README.md", Some("# core\n")),
            (
                "core/src/index.ts",
                Some("export const hello = () => \"hello\"\n"),
            ),
            ("private/secrets.md", Some("internal only\n")),
        ],
    );

    Fixture {
        sandbox,
        mono,
        pub_dir,
    }
}

pub struct MultiFixture {
    pub sandbox: Sandbox,
    pub mono: TestRepo,
    pub core_pub_dir: String,
    pub lib_pub_dir: String,
    pub core_pub: TestRepo,
    pub lib_pub: TestRepo,
}

/// Two subrepos, each with its own bare remote: `core/` at the top level and
/// `packages/lib/` nested one level down, so path handling is exercised both ways.
pub fn multi_fixture() -> MultiFixture {
    let sandbox = sandbox();
    let mono = make_repo(sandbox.path(), "mono");
    let core_pub_dir = make_bare_remote(sandbox.path(), "core-pub");
    let lib_pub_dir = make_bare_remote(sandbox.path(), "lib-pub");

    let core_block = subrepo_block(&[
        ("name", &toml_str("core")),
        ("path", &toml_str("core")),
        ("remote", &toml_str(&core_pub_dir)),
    ]);
    let lib_block = subrepo_block(&[
        ("name", &toml_str("lib")),
        ("path", &toml_str("packages/lib")),
        ("remote", &toml_str(&lib_pub_dir)),
    ]);
    write_config(&mono, &[&core_block, &lib_block]);

    mono.commit(
        "chore: initial monorepo",
        &[
            ("core/README.md", Some("# core\n")),
            (
                "core/src/index.ts",
                Some("export const hello = () => \"hello\"\n"),
            ),
            ("packages/lib/README.md", Some("# lib\n")),
            ("packages/lib/src/lib.ts", Some("export const lib = true\n")),
            ("private/secrets.md", Some("internal only\n")),
        ],
    );

    let core_pub = TestRepo::new(&core_pub_dir);
    let lib_pub = TestRepo::new(&lib_pub_dir);
    MultiFixture {
        sandbox,
        mono,
        core_pub_dir,
        lib_pub_dir,
        core_pub,
        lib_pub,
    }
}
