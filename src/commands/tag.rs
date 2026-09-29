//! Port of the corresponding TypeScript module — see docs/rust-port.md.
//!
//! Tag the standalone commit that corresponds to the current monorepo HEAD. A tag is a promise
//! that "this commit is what the monorepo says it is", so it may only be created when both
//! sides are already reflected in each other.

use std::path::Path;

use crate::config::ResolvedSubrepo;
use crate::core::exporter::{compute_exports, plan_export};
use crate::core::git::{git, push_ref};
use crate::core::sync_view::{SyncView, SyncViewOptions};
use crate::ops::{
    git_stderr, load_view, nothing_exists_yet, require_published, short, unplaced_claim,
};
use crate::report::{require_project, select_subrepos, Failure};

#[derive(clap::Args, Debug)]
pub struct TagArgs {
    #[arg(value_name = "subrepo", help = "Name of the subrepo to tag")]
    pub subrepo: String,

    #[arg(
        value_name = "tag",
        help = "Tag name to create on the standalone remote"
    )]
    pub tag: String,
}

pub fn run(args: &TagArgs) -> Result<(), Failure> {
    let project = require_project()?;
    let selected = select_subrepos(&project, Some(&args.subrepo))?;
    let Some(subrepo) = selected.first().copied() else {
        return Err(Failure::error(format!(
            "Unknown subrepo {}.",
            crate::report::json_quote(&args.subrepo)
        )));
    };
    let root = project.root.as_path();

    // A triangular subrepo has no commit monosplice may tag: `remote` is a fork whose branch it
    // rebuilds, and `upstream` belongs to someone else.
    if let Some(upstream) = &subrepo.upstream {
        return Err(Failure::error(format!(
            "{}: `upstream` is set, so {} is your fork and {upstream} is someone else's repository.
Tags belong to the upstream maintainers, and monosplice rebuilds the fork's {} branch on every push, so a tag there would soon point at abandoned history. No tag was created.
If you really want one on your fork, create it yourself:
  git push {} <sha>:refs/tags/{}",
            subrepo.name, subrepo.remote, subrepo.push_branch, subrepo.remote, args.tag
        )));
    }

    let view = load_view(root, subrepo, SyncViewOptions::default())
        .map_err(|failure| Failure::error(failure.message))?;
    require_published(root, subrepo, &view).map_err(|failure| Failure::error(failure.message))?;
    // require_published refuses when there is no published head, so this is always Some.
    let Some(pub_head) = view.pub_head.clone() else {
        return Err(Failure::error(nothing_exists_yet(subrepo)));
    };

    require_nothing_to_push(root, subrepo, &view)?;
    require_nothing_to_pull(subrepo, &view)?;
    require_tag_is_free(root, subrepo, &args.tag)?;

    push_ref(
        root,
        &subrepo.remote,
        &pub_head,
        &format!("refs/tags/{}", args.tag),
    )
    .map_err(|err| {
        Failure::error(format!(
            "{}: could not create tag {} on {}\n{}",
            subrepo.name,
            args.tag,
            subrepo.remote,
            git_stderr(&err)
        ))
    })?;

    println!(
        "✓ {}: tagged {} ({})",
        subrepo.name,
        args.tag,
        short(&pub_head)
    );
    Ok(())
}

fn require_nothing_to_push(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
) -> Result<(), Failure> {
    let candidates =
        plan_export(root, subrepo, view).map_err(|err| Failure::error(err.to_string()))?;
    let planned = compute_exports(root, subrepo, view, &candidates).map_err(|err| {
        Failure::error(format!(
            "{}: cannot tell what is unexported — {err}\nNo tag was created on {}.",
            subrepo.name, subrepo.remote
        ))
    })?;
    if planned.is_empty() {
        return Ok(());
    }

    Err(Failure::error(format!(
        "{}: {} commit(s) have not been exported yet, so {} does not match monorepo HEAD.
Tagging now would name a commit that is missing that work. No tag was created.
Run `monosplice push {}` first, then tag again.",
        subrepo.name,
        planned.len(),
        subrepo.remote,
        subrepo.name
    )))
}

fn require_nothing_to_pull(subrepo: &ResolvedSubrepo, view: &SyncView) -> Result<(), Failure> {
    if let Some(broken) = view.broken_source_refs.first() {
        return Err(Failure::error(unplaced_claim(
            subrepo,
            broken,
            "No tag was created.",
        )));
    }
    if view.unreflected_pub.is_empty() {
        return Ok(());
    }

    Err(Failure::error(format!(
        "{}: {} commit(s) on {} have not been imported yet.
Tagging now would name work the monorepo has never seen. No tag was created.
Run `monosplice pull {}` first, then tag again.",
        subrepo.name,
        view.unreflected_pub.len(),
        subrepo.remote,
        subrepo.name
    )))
}

fn require_tag_is_free(root: &Path, subrepo: &ResolvedSubrepo, tag: &str) -> Result<(), Failure> {
    let existing = git(
        root,
        &["ls-remote", &subrepo.remote, &format!("refs/tags/{tag}")],
    )
    .map_err(|err| {
        Failure::error(format!(
            "{}: cannot reach remote {}\n{}",
            subrepo.name,
            subrepo.remote,
            git_stderr(&err)
        ))
    })?;
    if existing.is_empty() {
        return Ok(());
    }

    let sha = existing.split('\t').next().unwrap_or("(unknown)");
    Err(Failure::error(format!(
        "{}: tag {tag} already exists on {} ({}).
Monosplice never moves an existing tag on the standalone repo. Pick another name, or delete it yourself with:
  git push {} :refs/tags/{tag}",
        subrepo.name,
        subrepo.remote,
        short(sha),
        subrepo.remote
    )))
}
