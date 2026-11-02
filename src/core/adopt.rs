//! Adoption — connecting the monorepo to a public branch it did not create — is an *import*-
//! side operation, so unlike export it is allowed to write the working tree and index. It
//! reuses the importer's patch machinery rather than plumbing trees directly, so the subrepo
//! directory ends up as a normal part of the monorepo commit.

use std::path::Path;

use crate::config::ResolvedSubrepo;
use crate::core::git::{git, git_buffer, git_with, GitError, GitOpts};
use crate::core::sync_view::pull_source;
use crate::core::trailers::{append_trailer, ORIGIN_TRAILER};

/// The commit that anchors the monorepo to a public branch it did not create. One shape for
/// every route in: a folder attach that also writes the config entry records exactly the same
/// anchor as one that only makes first contact.
pub fn adopt_message(subrepo: &ResolvedSubrepo, pub_head: &str) -> String {
    let short: String = pub_head.chars().take(10).collect();
    let subject = format!(
        "Adopt {} from {} @ {short}\n",
        subrepo.name,
        pull_source(subrepo)
    );
    append_trailer(&subject, ORIGIN_TRAILER, pub_head)
}

/// Paths where two trees disagree, as the user would see them inside the subrepo.
pub fn differing_paths(
    root: &Path,
    from_tree: &str,
    to_tree: &str,
) -> Result<Vec<String>, GitError> {
    let out = git(
        root,
        &["diff-tree", "-r", "--name-only", from_tree, to_tree],
    )?;
    if out.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(out.split('\n').map(str::to_string).collect())
    }
}

/// Stage the change from one tree to another inside the subrepo directory.
pub fn apply_tree_into(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    from_tree: &str,
    to_tree: &str,
) -> Result<(), GitError> {
    let patch = git_buffer(
        root,
        &["diff-tree", "--binary", "-M", "-p", from_tree, to_tree],
        GitOpts::default(),
    )?;
    if patch.is_empty() {
        return Ok(());
    }
    let directory = format!("--directory={}", subrepo.path);
    git_with(
        root,
        &["apply", "--index", &directory],
        GitOpts {
            env: &[],
            input: Some(&patch),
        },
    )?;
    Ok(())
}

/// Commit whatever adopt staged. `--allow-empty` because the matching-trees case records the
/// baseline without changing a byte: the Origin trailer is the whole point of the commit.
pub fn commit_staged(root: &Path, message: &str) -> Result<String, GitError> {
    git(
        root,
        &["commit", "--allow-empty", "--no-verify", "-m", message],
    )?;
    git(root, &["rev-parse", "HEAD"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::git::EMPTY_TREE;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DATE: AtomicU64 = AtomicU64::new(1_800_000_000);

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(name: &str) -> Sandbox {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
            let dir = std::env::temp_dir().join(format!("ms-adopt-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Sandbox(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sh(cwd: &Path, cmd: &str) -> String {
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        fs::create_dir_all(&dir).unwrap();
        sh(&dir, "git init -q -b main .");
        sh(
            &dir,
            "git config user.name 'Mono Dev' && git config user.email mono@example.com",
        );
        dir
    }

    fn commit(dir: &Path, message: &str) -> String {
        let ts = DATE.fetch_add(61, Ordering::SeqCst);
        sh(
            dir,
            &format!(
                "git add -A && GIT_AUTHOR_DATE='{ts} +0000' GIT_COMMITTER_DATE='{ts} +0000' \
                 git commit -q --allow-empty -m '{message}'"
            ),
        );
        sh(dir, "git rev-parse HEAD")
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let file = dir.join(rel);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(file, content).unwrap();
    }

    fn subrepo(upstream: Option<&str>) -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: "core".to_string(),
            path: "vendor/core".to_string(),
            remote: "git@example.com:me/core-fork.git".to_string(),
            upstream: upstream.map(str::to_string),
            branch: "main".to_string(),
            push_branch: "main".to_string(),
            exclude: Vec::new(),
            rewrite_message: None,
            transform: None,
            scan: None,
            monorepo_id: None,
        }
    }

    #[test]
    fn adopt_message_pins_the_anchor_format() {
        let head = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            adopt_message(&subrepo(None), head),
            format!(
                "Adopt core from git@example.com:me/core-fork.git @ 0123456789\n\nMonosplice-Origin: {head}\n"
            )
        );
        // Triangular: the anchor names the repository the content actually came from.
        assert_eq!(
            adopt_message(&subrepo(Some("git@example.com:acme/core.git")), head),
            format!(
                "Adopt core from git@example.com:acme/core.git @ 0123456789\n\nMonosplice-Origin: {head}\n"
            )
        );
    }

    #[test]
    fn differing_paths_lists_the_tree_diff_and_nothing_when_equal() {
        let sb = Sandbox::new("differing");
        let repo = init_repo(sb.path(), "pub");
        write(&repo, "a.txt", "a\n");
        write(&repo, "b/c.txt", "c\n");
        let p1 = commit(&repo, "first");
        write(&repo, "b/c.txt", "c2\n");
        let p2 = commit(&repo, "second");

        assert_eq!(
            differing_paths(&repo, EMPTY_TREE, &p1).unwrap(),
            vec!["a.txt".to_string(), "b/c.txt".to_string()]
        );
        assert_eq!(
            differing_paths(&repo, &p1, &p2).unwrap(),
            vec!["b/c.txt".to_string()]
        );
        assert!(differing_paths(&repo, &p1, &p1).unwrap().is_empty());
    }

    #[test]
    fn apply_tree_into_stages_exactly_the_tree_diff_under_the_subrepo_path() {
        let sb = Sandbox::new("apply");
        let mono = init_repo(sb.path(), "mono");
        write(&mono, "README.md", "root\n");
        commit(&mono, "mono init");

        let pubr = init_repo(sb.path(), "pub");
        write(&pubr, "a.txt", "a\n");
        write(&pubr, "b/c.txt", "c\n");
        let p1 = commit(&pubr, "first");
        write(&pubr, "b/c.txt", "c2\n");
        let p2 = commit(&pubr, "second");

        sh(
            &mono,
            &format!(
                "git fetch -q --no-tags {} +refs/heads/main:refs/monosplice/core/pub",
                pubr.display()
            ),
        );

        let s = subrepo(None);
        apply_tree_into(&mono, &s, EMPTY_TREE, &p1).unwrap();
        assert_eq!(
            git(&mono, &["diff", "--cached", "--name-only"]).unwrap(),
            "vendor/core/a.txt\nvendor/core/b/c.txt"
        );
        assert_eq!(
            fs::read_to_string(mono.join("vendor/core/a.txt")).unwrap(),
            "a\n"
        );

        let sha = commit_staged(&mono, "Adopt core\n\nMonosplice-Origin: x\n").unwrap();
        assert_eq!(sha, git(&mono, &["rev-parse", "HEAD"]).unwrap());
        assert_eq!(sha.len(), 40);
        assert_eq!(git(&mono, &["status", "--porcelain"]).unwrap(), "");

        // A second adoption step stages only what changed between the two trees.
        apply_tree_into(&mono, &s, &p1, &p2).unwrap();
        assert_eq!(
            git(&mono, &["diff", "--cached", "--name-only"]).unwrap(),
            "vendor/core/b/c.txt"
        );
        assert_eq!(
            fs::read_to_string(mono.join("vendor/core/b/c.txt")).unwrap(),
            "c2\n"
        );
        commit_staged(&mono, "second step\n").unwrap();

        // Matching trees: nothing to apply, and the empty commit still records the anchor.
        apply_tree_into(&mono, &s, &p2, &p2).unwrap();
        assert_eq!(
            git(&mono, &["diff", "--cached", "--name-only"]).unwrap(),
            ""
        );
        let head_before = git(&mono, &["rev-parse", "HEAD"]).unwrap();
        let empty = commit_staged(&mono, "Adopt core\n\nMonosplice-Origin: y\n").unwrap();
        assert_ne!(empty, head_before);
        assert_eq!(
            git(&mono, &["log", "-1", "--format=%B"]).unwrap(),
            "Adopt core\n\nMonosplice-Origin: y\n"
        );
    }
}
