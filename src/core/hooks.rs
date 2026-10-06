//! Running the configured shell hooks (see the "Hooks are shell commands now" section of
//! docs/rust-port.md).
//!
//! The TypeScript config carried JavaScript functions; a Rust binary cannot call those, so
//! `rewrite-message`, `scan` and `transform` are shell commands run via `sh -c`. Everything
//! else about them is unchanged: they run once per exported commit, before anything is
//! pushed, and a non-zero exit aborts the whole export.
//!
//! `scan`/`transform` need a real directory to look at, so the outgoing (post-exclude) tree
//! is materialized into a temp dir. Gitlink entries have no blob content and are carried
//! through untouched, exactly like the TS `FileMap` path did.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::core::git::{hash_object, read_blob, TreeEntry};

/// A configured hook rejected (scan) or failed while processing one monorepo commit.
///
/// `hook` is the config key as the user spells it: `scan`, `transform`, `rewrite-message`.
#[derive(Debug)]
pub struct HookError {
    pub hook: &'static str,
    pub mono_sha: String,
    pub subrepo: String,
    pub detail: String,
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} hook rejected {} commit {}: {}",
            self.hook, self.subrepo, self.mono_sha, self.detail
        )
    }
}

impl std::error::Error for HookError {}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_name(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("monosplice-{tag}-{}-{n}", std::process::id()))
}

/// What a hook's non-zero exit says when it printed nothing on stderr.
fn status_detail(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exited with status {code}"),
        None => "exited with status unknown".to_string(),
    }
}

/// The original, pre-rewrite commit message on disk. Hooks read it through
/// `MONOSPLICE_MESSAGE_FILE`; it lives outside the materialized tree so a `transform` can
/// never sweep it into the exported tree.
pub struct MessageFile(PathBuf);

impl MessageFile {
    pub fn new(message: &str) -> Result<Self, String> {
        let path = temp_name("msg");
        fs::write(&path, message.as_bytes())
            .map_err(|e| format!("could not write hook message file {}: {e}", path.display()))?;
        Ok(MessageFile(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for MessageFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Run the `rewrite-message` hook: original message on stdin, rewritten message on stdout.
/// Trailers are appended by the caller *after* this runs, exactly as in the TS.
pub fn run_rewrite_message(
    cmd: &str,
    root: &Path,
    subrepo: &str,
    mono_sha: &str,
    message: &str,
) -> Result<String, HookError> {
    let fail = |detail: String| HookError {
        hook: "rewrite-message",
        mono_sha: mono_sha.to_string(),
        subrepo: subrepo.to_string(),
        detail,
    };

    let message_file = MessageFile::new(message).map_err(fail)?;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(root)
        .env("MONOSPLICE_SUBREPO", subrepo)
        .env("MONOSPLICE_MONO_SHA", mono_sha)
        .env("MONOSPLICE_MESSAGE_FILE", message_file.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| fail(e.to_string()))?;

    // Feeding stdin from another thread is what keeps a long message from deadlocking
    // against a hook that writes its output before draining its input.
    let writer = child.stdin.take().map(|mut stdin| {
        let data = message.as_bytes().to_vec();
        thread::spawn(move || {
            let _ = stdin.write_all(&data);
        })
    });

    let out = child.wait_with_output().map_err(|e| fail(e.to_string()))?;
    if let Some(handle) = writer {
        let _ = handle.join();
    }

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(fail(if stderr.is_empty() {
            status_detail(&out.status)
        } else {
            stderr
        }));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The outgoing tree, on disk, plus the entries that were never written there.
///
/// Dropping this removes the directory: a hook run must not leave anything behind, and the
/// tree is re-read into the object db before the guard goes out of scope.
pub struct MaterializedTree {
    dir: PathBuf,
    passthrough: Vec<TreeEntry>,
}

impl MaterializedTree {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Entries with no blob content (gitlinks) — not on disk, spliced back in afterwards.
    pub fn passthrough(&self) -> &[TreeEntry] {
        &self.passthrough
    }
}

impl Drop for MaterializedTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Write flat tree entries into a fresh temp dir so a hook can look at real files.
///
/// Blobs get their exec bit from the git mode, symlinks (120000) become real symlinks, and
/// anything without blob content (a gitlink, mode 160000) is *not* written — it is handed
/// back for passthrough so a submodule survives a transform untouched.
pub fn materialize_tree(root: &Path, entries: &[TreeEntry]) -> Result<MaterializedTree, String> {
    let dir = temp_name("tree");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create hook work dir {}: {e}", dir.display()))?;
    // Bound to a guard immediately: a failure halfway through still cleans up.
    let mut tree = MaterializedTree {
        dir,
        passthrough: Vec::new(),
    };

    for e in entries {
        if e.kind != "blob" {
            tree.passthrough.push(e.clone());
            continue;
        }
        let target = tree.dir.join(&e.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
        }
        let data = read_blob(root, &e.sha).map_err(|err| err.to_string())?;
        if e.mode == "120000" {
            write_symlink(&data, &target)?;
        } else {
            fs::write(&target, &data)
                .map_err(|err| format!("could not write {}: {err}", target.display()))?;
            set_exec(&target, e.mode == "100755")?;
        }
    }
    Ok(tree)
}

/// Run `scan` or `transform` with the materialized tree as its working directory.
///
/// `root` is not the cwd here: the spec puts the hook *inside* the outgoing tree so a scanner
/// can grep relative paths and a transform can edit files in place.
pub fn run_tree_hook(
    hook: &'static str,
    cmd: &str,
    tree_dir: &Path,
    _root: &Path,
    subrepo: &str,
    mono_sha: &str,
    message_file: &Path,
) -> Result<(), HookError> {
    let fail = |detail: String| HookError {
        hook,
        mono_sha: mono_sha.to_string(),
        subrepo: subrepo.to_string(),
        detail,
    };

    // stdout is captured, never inherited: `status --json` has to stay pipeable no matter
    // what a hook prints.
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(tree_dir)
        .env("MONOSPLICE_SUBREPO", subrepo)
        .env("MONOSPLICE_MONO_SHA", mono_sha)
        .env("MONOSPLICE_MESSAGE_FILE", message_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| fail(e.to_string()))?;

    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(fail(if stderr.is_empty() {
        status_detail(&out.status)
    } else {
        stderr
    }))
}

/// Read a (possibly transform-modified) directory back into flat tree entries, writing every
/// blob into `repo_root`'s object db. Modes come from the filesystem: symlink → 120000,
/// exec bit → 100755, else 100644. Nothing is skipped — dotfiles a transform added are part
/// of the export.
pub fn rehash_tree(repo_root: &Path, tree_dir: &Path) -> Result<Vec<TreeEntry>, String> {
    let mut entries = Vec::new();
    walk(repo_root, tree_dir, "", &mut entries)?;
    Ok(entries)
}

fn walk(repo_root: &Path, base: &Path, rel: &str, out: &mut Vec<TreeEntry>) -> Result<(), String> {
    let dir = if rel.is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    };
    let mut names: Vec<std::ffi::OsString> = fs::read_dir(&dir)
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?;
    names.sort();

    for name in names {
        let child = dir.join(&name);
        let Some(name) = name.to_str() else {
            return Err(format!(
                "hook produced a path that is not valid UTF-8: {}",
                child.display()
            ));
        };
        let rel_child = if rel.is_empty() {
            name.to_string()
        } else {
            format!("{rel}/{name}")
        };
        let md = fs::symlink_metadata(&child)
            .map_err(|e| format!("could not stat {}: {e}", child.display()))?;
        let ft = md.file_type();
        if ft.is_symlink() {
            let target = read_symlink_bytes(&child)?;
            out.push(TreeEntry {
                mode: "120000".to_string(),
                kind: "blob".to_string(),
                sha: hash_object(repo_root, &target).map_err(|e| e.to_string())?,
                path: rel_child,
            });
        } else if ft.is_dir() {
            walk(repo_root, base, &rel_child, out)?;
        } else if ft.is_file() {
            let data =
                fs::read(&child).map_err(|e| format!("could not read {}: {e}", child.display()))?;
            out.push(TreeEntry {
                mode: file_mode(&md).to_string(),
                kind: "blob".to_string(),
                sha: hash_object(repo_root, &data).map_err(|e| e.to_string())?,
                path: rel_child,
            });
        } else {
            return Err(format!(
                "hook left something git cannot store at {}: not a file, directory or symlink",
                child.display()
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(md: &fs::Metadata) -> &'static str {
    use std::os::unix::fs::PermissionsExt;
    if md.permissions().mode() & 0o111 != 0 {
        "100755"
    } else {
        "100644"
    }
}

#[cfg(not(unix))]
fn file_mode(_md: &fs::Metadata) -> &'static str {
    "100644"
}

#[cfg(unix)]
fn set_exec(path: &Path, exec: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if exec { 0o755 } else { 0o644 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| format!("could not chmod {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn set_exec(_path: &Path, _exec: bool) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn write_symlink(target: &[u8], link: &Path) -> Result<(), String> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(OsStr::from_bytes(target), link)
        .map_err(|e| format!("could not create symlink {}: {e}", link.display()))
}

#[cfg(not(unix))]
fn write_symlink(_target: &[u8], link: &Path) -> Result<(), String> {
    Err(format!(
        "cannot materialize the symlink {} for a hook: monosplice hooks need a Unix platform",
        link.display()
    ))
}

#[cfg(unix)]
fn read_symlink_bytes(path: &Path) -> Result<Vec<u8>, String> {
    use std::os::unix::ffi::OsStrExt;
    let target =
        fs::read_link(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    Ok(target.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn read_symlink_bytes(path: &Path) -> Result<Vec<u8>, String> {
    Err(format!(
        "cannot read the symlink {} a hook produced: monosplice hooks need a Unix platform",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::git::{build_tree, git, ls_tree_recursive};

    fn hermetic() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        });
    }

    struct Repo(PathBuf);

    impl Repo {
        fn new(tag: &str) -> Self {
            hermetic();
            let dir = temp_name(&format!("hooks-test-{tag}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create repo dir");
            let r = Repo(dir);
            r.sh("git init -q -b main .");
            r.sh("git config user.name 'Mono Author' && git config user.email mono@example.test");
            r
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn sh(&self, cmd: &str) -> String {
            let out = Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(&self.0)
                .env("GIT_AUTHOR_DATE", "1767225661 +0000")
                .env("GIT_COMMITTER_DATE", "1767225661 +0000")
                .output()
                .expect("spawn sh");
            assert!(
                out.status.success(),
                "{cmd}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hook_error_renders_like_the_ts_message() {
        let err = HookError {
            hook: "scan",
            mono_sha: "abc123".to_string(),
            subrepo: "core".to_string(),
            detail: "found a secret".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "scan hook rejected core commit abc123: found a secret"
        );
    }

    #[test]
    fn rewrite_message_pipes_the_message_through_stdout() {
        let repo = Repo::new("rewrite");
        let out = run_rewrite_message("sed s/foo/bar/", repo.path(), "core", "abc", "foo thing\n")
            .expect("hook runs");
        assert_eq!(out, "bar thing\n");
    }

    #[test]
    fn rewrite_message_sees_the_hook_env_and_message_file() {
        let repo = Repo::new("rewrite-env");
        let out = run_rewrite_message(
            "printf '%s|%s|' \"$MONOSPLICE_SUBREPO\" \"$MONOSPLICE_MONO_SHA\"; cat \"$MONOSPLICE_MESSAGE_FILE\"",
            repo.path(),
            "core",
            "deadbeef",
            "original subject\n",
        )
        .expect("hook runs");
        assert_eq!(out, "core|deadbeef|original subject\n");
    }

    #[test]
    fn a_failing_rewrite_message_surfaces_its_stderr() {
        let repo = Repo::new("rewrite-fail");
        let err = run_rewrite_message("echo nope 1>&2; exit 3", repo.path(), "core", "abc", "m")
            .expect_err("hook fails");
        assert_eq!(err.hook, "rewrite-message");
        assert_eq!(err.detail, "nope");
        assert_eq!(
            err.to_string(),
            "rewrite-message hook rejected core commit abc: nope"
        );
    }

    #[test]
    fn a_silent_failure_falls_back_to_the_exit_status() {
        let repo = Repo::new("rewrite-silent");
        let err =
            run_rewrite_message("exit 7", repo.path(), "core", "abc", "m").expect_err("hook fails");
        assert_eq!(err.detail, "exited with status 7");
    }

    #[test]
    fn materialize_writes_modes_symlinks_and_keeps_gitlinks_off_disk() {
        let repo = Repo::new("materialize");
        repo.sh("mkdir -p bin sub && printf 'plain\n' > README.md && printf 'run\n' > bin/run.sh && chmod +x bin/run.sh && ln -s README.md link && printf 'x\n' > sub/x.txt");
        repo.sh("git add -A && git commit -q -m first");
        let mut entries = ls_tree_recursive(repo.path(), "HEAD").expect("ls-tree");
        entries.push(TreeEntry {
            mode: "160000".to_string(),
            kind: "commit".to_string(),
            sha: "0".repeat(40),
            path: "vendor/dep".to_string(),
        });

        let tree = materialize_tree(repo.path(), &entries).expect("materialize");
        let dir = tree.dir().to_path_buf();
        assert_eq!(
            fs::read_to_string(dir.join("README.md")).unwrap(),
            "plain\n"
        );
        assert_eq!(
            fs::read_link(dir.join("link")).unwrap().to_str(),
            Some("README.md")
        );
        assert!(dir.join("sub/x.txt").is_file());
        // A gitlink is never written to disk...
        assert!(!dir.join("vendor").exists());
        // ...it comes back for passthrough instead.
        assert_eq!(tree.passthrough().len(), 1);
        assert_eq!(tree.passthrough()[0].path, "vendor/dep");

        // Modes round-trip through a rehash, so an untouched tree is the same tree.
        let rehashed = rehash_tree(repo.path(), &dir).expect("rehash");
        let modes: Vec<(&str, &str)> = rehashed
            .iter()
            .map(|e| (e.path.as_str(), e.mode.as_str()))
            .collect();
        assert!(modes.contains(&("bin/run.sh", "100755")));
        assert!(modes.contains(&("README.md", "100644")));
        assert!(modes.contains(&("link", "120000")));

        let original = build_tree(repo.path(), &entries[..entries.len() - 1]).expect("build");
        let round_tripped = build_tree(repo.path(), &rehashed).expect("build");
        assert_eq!(original, round_tripped);

        drop(tree);
        assert!(!dir.exists(), "the guard removes the work dir");
    }

    #[test]
    fn a_tree_hook_runs_inside_the_materialized_dir_with_the_env_set() {
        let repo = Repo::new("tree-hook-env");
        repo.sh("printf 'hello\n' > README.md && git add -A && git commit -q -m first");
        let entries = ls_tree_recursive(repo.path(), "HEAD").expect("ls-tree");
        let tree = materialize_tree(repo.path(), &entries).expect("materialize");
        let msg = MessageFile::new("subject line\n").expect("message file");

        run_tree_hook(
            "transform",
            "printf '%s %s\n' \"$MONOSPLICE_SUBREPO\" \"$MONOSPLICE_MONO_SHA\" > seen.txt; cat README.md >> seen.txt; cat \"$MONOSPLICE_MESSAGE_FILE\" >> seen.txt",
            tree.dir(),
            repo.path(),
            "core",
            "cafe01",
            msg.path(),
        )
        .expect("hook runs");

        assert_eq!(
            fs::read_to_string(tree.dir().join("seen.txt")).unwrap(),
            "core cafe01\nhello\nsubject line\n"
        );
    }

    #[test]
    fn a_failing_tree_hook_carries_its_trimmed_stderr() {
        let repo = Repo::new("tree-hook-fail");
        let tree = materialize_tree(repo.path(), &[]).expect("materialize");
        let msg = MessageFile::new("m").expect("message file");
        let err = run_tree_hook(
            "scan",
            "echo '  found AWS key  ' 1>&2; exit 1",
            tree.dir(),
            repo.path(),
            "core",
            "abc",
            msg.path(),
        )
        .expect_err("hook rejects");
        assert_eq!(err.hook, "scan");
        assert_eq!(err.detail, "found AWS key");

        let silent = run_tree_hook(
            "scan",
            "exit 2",
            tree.dir(),
            repo.path(),
            "core",
            "abc",
            msg.path(),
        )
        .expect_err("hook rejects");
        assert_eq!(silent.detail, "exited with status 2");
    }

    #[test]
    fn rehash_includes_dotfiles_and_nested_additions() {
        let repo = Repo::new("rehash-dotfiles");
        let tree = materialize_tree(repo.path(), &[]).expect("materialize");
        fs::create_dir_all(tree.dir().join("deep/nest")).unwrap();
        fs::write(tree.dir().join(".hidden"), "h\n").unwrap();
        fs::write(tree.dir().join("deep/nest/.keep"), "k\n").unwrap();

        let entries = rehash_tree(repo.path(), tree.dir()).expect("rehash");
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec![".hidden", "deep/nest/.keep"]);
        // The blobs really landed in the object db.
        assert_eq!(
            git(repo.path(), &["cat-file", "blob", &entries[0].sha]).unwrap(),
            "h"
        );
    }

    #[test]
    fn the_message_file_is_removed_when_its_guard_drops() {
        let msg = MessageFile::new("body\n").expect("message file");
        let path = msg.path().to_path_buf();
        assert_eq!(fs::read_to_string(&path).unwrap(), "body\n");
        drop(msg);
        assert!(!path.exists());
    }
}
