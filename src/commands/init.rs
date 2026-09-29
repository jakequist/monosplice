//! Port of the corresponding TypeScript module — see docs/rust-port.md.
//!
//! `init` scaffolds the config. The TS wrote a JavaScript module; the Rust port writes
//! `monosplice.toml`, and the subrepo template is commented out on purpose: an
//! array-of-tables cannot be appended to a `subrepos = []` that is already there, so the
//! example block has to be a comment for `attach` to have somewhere to write.
//!
//! The one live key is `id`: a new monorepo gets its own from the start, so every claim it ever
//! writes on a standalone branch says whose it is.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{find_config, CONFIG_FILENAME};
use crate::core::git::git_ok;
use crate::report::Failure;

/// The scaffold, with this monorepo's `id`.
pub fn template(id: &str) -> String {
    format!(
        r#"# Monosplice configuration.
# Docs: https://github.com/jakequist/monosplice
#
# `id` names this monorepo in the trailers it writes on standalone repos
# (`Monosplice-Monorepo: <id>`), so a repository that another monorepo also
# publishes, or vendors, can tell whose commit is whose. Keep it for good:
# never change it, and never copy it into another monorepo's config.
id = "{id}"

# Each subrepo is one [[subrepos]] block:
#
# [[subrepos]]
# path = "packages/my-lib"
# remote = "git@github.com:me/my-lib.git"
# branch = "main"
# exclude = []
"#
    )
}

/// 128 random bits as 32 hex digits. `RandomState` is seeded from the operating system's
/// randomness, which is all a name that must not collide needs; the clock and the process id are
/// mixed in so two keys drawn in one process still differ.
pub fn fresh_monorepo_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let half = |salt: u64| {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_u32(std::process::id());
        h.write_u64(salt);
        h.finish()
    };
    format!("{:016x}{:016x}", half(1), half(2))
}

#[derive(clap::Args, Debug)]
pub struct InitArgs {}

pub fn run(_args: &InitArgs) -> Result<(), Failure> {
    let cwd = std::env::current_dir()
        .map_err(|err| Failure::error(format!("Cannot read the current directory: {err}")))?;
    run_in(&cwd)
}

fn run_in(cwd: &Path) -> Result<(), Failure> {
    let existing = find_config(cwd).map_err(|err| Failure::error(err.0))?;
    if let Some(path) = existing {
        println!("Already initialized: {}", path.display());
        return Ok(());
    }

    if !git_ok(cwd, &["rev-parse", "--is-inside-work-tree"]) {
        return Err(Failure::error(
            "Not inside a git repository. Run `git init` first — monosplice manages subdirectories of a git repo.",
        ));
    }

    let target = cwd.join(CONFIG_FILENAME);
    std::fs::write(&target, template(&fresh_monorepo_id())).map_err(|err| {
        Failure::error(format!(
            "Could not write {}: {err}\nNothing was changed.",
            target.display()
        ))
    })?;

    println!("Created {}", target.display());
    println!("Add your subrepos to the config, then run `monosplice push <name>` to publish one");
    println!(
        "(or skip the hand-editing: `monosplice attach <folder> <git-url>` writes the entry and"
    );
    println!("makes first contact for you, whichever side already has content).");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{is_valid_monorepo_id, monorepo_id_of};

    #[test]
    fn the_template_carries_a_commented_subrepos_example() {
        let text = template("abc");
        let commented = |needle: &str| {
            text.lines()
                .any(|l| l.trim_start().starts_with('#') && l.contains(needle))
        };
        assert!(commented("[[subrepos]]"), "{text}");
        assert!(commented("path = "), "{text}");
        assert!(commented("remote = "), "{text}");
        assert!(commented("branch = "), "{text}");
        assert!(commented("exclude = "), "{text}");
    }

    /// The template must load through the real loader as "nothing attached yet", or `attach`
    /// would have no valid file to append its first entry to.
    #[test]
    fn the_template_is_a_valid_empty_config() {
        let id = fresh_monorepo_id();
        let text = template(&id);
        let resolved = crate::config::resolve_config(&text, Path::new("/repo/monosplice.toml"))
            .expect("the scaffold must be valid TOML the loader accepts");
        assert!(resolved.is_empty());
        assert_eq!(monorepo_id_of(&text).as_deref(), Some(id.as_str()));
    }

    #[test]
    fn fresh_ids_are_valid_and_distinct() {
        let a = fresh_monorepo_id();
        let b = fresh_monorepo_id();
        assert_eq!(a.len(), 32);
        assert!(is_valid_monorepo_id(&a), "{a}");
        assert_ne!(a, b);
    }
}
