//! Port of `src/commands/detach.ts` — see docs/rust-port.md.
//!
//! Detaching is a config edit, not a sync: nothing in this command opens a network
//! connection, and nothing writes until every refusal above it has passed.

use crate::config::{Project, ResolvedSubrepo};
use crate::core::adopt::commit_staged;
use crate::core::git::git;
use crate::core::importer::read_sequencer;
use crate::core::paths::normalize_subrepo_path;
use crate::core::sync_view::pull_source;
use crate::core::vendor::{
    check_config_edit_preconditions, delete_it_yourself, remove_config_entry,
};
use crate::report::{configured_names, json_quote, require_project, Failure};

#[derive(clap::Args, Debug)]
pub struct DetachArgs {
    #[arg(
        value_name = "subrepo",
        help = "Subrepo to stop tracking (its name, or its folder)"
    )]
    pub subrepo: String,
}

pub fn run(args: &DetachArgs) -> Result<(), Failure> {
    let project = require_project()?;
    let root = project.root.clone();

    let Some(entry) = find_entry(&project, &args.subrepo) else {
        return Err(Failure::error(format!(
            "Unknown subrepo {}. Configured subrepos: {}\nNothing was changed.",
            json_quote(&args.subrepo),
            configured_names(&project),
        )));
    };

    let retry = format!("monosplice detach {}", args.subrepo);

    // Everything below writes something. Nothing above did.
    if let Some(state) = read_sequencer(&root) {
        if state.subrepo == entry.name {
            return Err(Failure::error(format!(
                "A pull of {} is unfinished, so detaching it now would strand the import mid-flight.\nNothing was changed. Finish it with `monosplice pull --continue`, or throw it away with `monosplice pull --abort`, then run `{retry}` again.",
                entry.name,
            )));
        }
    }
    if let Some(problem) = check_config_edit_preconditions(&root, &retry, "Detaching") {
        return Err(Failure::error(problem));
    }

    let failure =
        remove_config_entry(&project, entry).map_err(|err| Failure::error(err.to_string()))?;
    if let Some(failure) = failure {
        let (log, error) = delete_it_yourself(&project.config_path, entry, &failure);
        println!("{log}");
        return Err(Failure::error(error));
    }

    let source = pull_source(entry);
    let config_path = project.config_path.display().to_string();
    git(&root, &["add", "--", &config_path]).map_err(|err| Failure::error(err.to_string()))?;
    commit_staged(
        &root,
        &format!("Detach {}: stop tracking {source}", entry.name),
    )
    .map_err(|err| Failure::error(err.to_string()))?;

    println!(
        "✓ detached {} — {config_path} no longer tracks {source}",
        entry.name
    );
    println!(
        "  {}/ is kept exactly as it is, and every commit stays in your monorepo history.",
        entry.path
    );
    println!(
        "  The Monosplice trailers on those commits are inert now: nothing is pushed or pulled for {} any more.",
        entry.name
    );
    println!("  To connect it again later:");
    println!(
        "    monosplice attach {} {source}{}",
        entry.path,
        match entry.upstream {
            None => String::new(),
            Some(_) => format!(" --fork {}", entry.remote),
        }
    );
    Ok(())
}

/// The configured subrepo this argument names — by name, or failing that by path.
fn find_entry<'a>(project: &'a Project, arg: &str) -> Option<&'a ResolvedSubrepo> {
    if let Some(by_name) = project.subrepos.iter().find(|s| s.name == arg) {
        return Some(by_name);
    }
    let sub_path = normalize_subrepo_path(arg).ok()?;
    project.subrepos.iter().find(|s| s.path == sub_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn subrepo(name: &str, path: &str) -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: name.to_string(),
            path: path.to_string(),
            remote: format!("git@example.test:{name}.git"),
            upstream: None,
            branch: "main".to_string(),
            push_branch: "main".to_string(),
            exclude: Vec::new(),
            rewrite_message: None,
            transform: None,
            scan: None,
            monorepo_id: None,
        }
    }

    fn project() -> Project {
        Project {
            root: PathBuf::from("/repo"),
            config_path: PathBuf::from("/repo/monosplice.toml"),
            subrepos: vec![subrepo("core", "core"), subrepo("lib", "packages/lib")],
            id: None,
        }
    }

    #[test]
    fn an_entry_is_found_by_name_first() {
        let project = project();
        assert_eq!(find_entry(&project, "core").unwrap().name, "core");
        assert_eq!(find_entry(&project, "lib").unwrap().name, "lib");
    }

    #[test]
    fn an_entry_is_found_by_its_folder_normalized() {
        let project = project();
        assert_eq!(find_entry(&project, "packages/lib").unwrap().name, "lib");
        assert_eq!(find_entry(&project, "./packages/lib/").unwrap().name, "lib");
        assert_eq!(find_entry(&project, "/core").unwrap().name, "core");
    }

    #[test]
    fn an_unnormalizable_argument_is_simply_unknown() {
        let project = project();
        assert!(find_entry(&project, "..").is_none());
        assert!(find_entry(&project, "/").is_none());
        assert!(find_entry(&project, "nope").is_none());
    }
}
