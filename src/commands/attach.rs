//! Port of `src/commands/attach.ts` — see docs/rust-port.md.
//!
//! One command with two halves, and the config decides which: a folder the config already
//! names is *first contact only* (nothing here may touch `monosplice.toml`), and a folder it
//! does not name gets the entry written first, then the same first-contact move.

use std::path::Path;

use crate::config::{Project, ResolvedSubrepo};
use crate::core::adopt::{adopt_message, apply_tree_into, commit_staged, differing_paths};
use crate::core::filter::{filtered_subtree, has_committed_files};
use crate::core::git::{
    fetch_branch, git, ls_remote_branch, probe_push_access, rev_parse, EMPTY_TREE,
};
use crate::core::importer::{check_import_preconditions, read_sequencer, run_import};
use crate::core::paths::normalize_subrepo_path;
use crate::core::sync_view::{pull_source, remote_tracking_ref, SyncView, SyncViewOptions};
use crate::core::vendor::{
    check_config_edit_preconditions, check_free_slot, paste_it_yourself, write_config_entry,
    SlotHints, VendorEntry,
};
use crate::ops::{
    confirm_first_publish, first_publish, git_message, git_stderr, load_view, nothing_exists_yet,
    pull_in_progress_message, report_import_failure, short, upstream_has_no_branch,
    ConfirmFirstPublishOptions,
};
use crate::report::{configured_names, warn, Failure, SubrepoFailure};

/// How `attach` tells the user to pick a different name or a different directory.
fn attach_hints() -> SlotHints {
    SlotHints {
        rename: "Attach it under another name with `--name <name>`".to_string(),
        relocate: "Attach it at a directory that is not already part of a subrepo.".to_string(),
    }
}

#[derive(clap::Args, Debug)]
pub struct AttachArgs {
    #[arg(
        value_name = "folder",
        help = "Directory in this monorepo to connect (or the name of a configured subrepo)"
    )]
    pub folder: String,

    #[arg(
        value_name = "url",
        help = "Git URL of the standalone repository. Optional when <folder> is already in your config"
    )]
    pub url: Option<String>,

    #[arg(
        long,
        value_name = "name",
        help = "Subrepo name (default: the last segment of <folder>)"
    )]
    pub name: Option<String>,

    #[arg(
        long,
        value_name = "branch",
        help = "Branch to sync on both sides (default: main)"
    )]
    pub branch: Option<String>,

    #[arg(
        short = 'y',
        long,
        help = "Answer the first-publish confirmation with yes (required in scripts and CI)"
    )]
    pub yes: bool,

    #[arg(
        long = "export-history",
        help = "First publish only: replay every monorepo commit that touched <folder> instead of one baseline commit (not to be confused with --import-history, which goes the other way)"
    )]
    pub export_history: bool,

    #[arg(
        long = "import-history",
        help = "Replay every commit from the standalone repo into <folder> instead of recording one snapshot commit (not to be confused with --export-history, which goes the other way)"
    )]
    pub import_history: bool,

    #[arg(
        long,
        help = "When both sides have content, replace <folder> with the standalone tree"
    )]
    pub theirs: bool,

    #[arg(
        long,
        value_name = "fork",
        help = "Your fork of the repository: pull from <url>, push patches to this remote"
    )]
    pub fork: Option<String>,
}

pub fn run(args: &AttachArgs) -> Result<(), Failure> {
    let project = crate::report::require_project()?;

    // Which half of `attach` this is, decided by the config — never by a flag.
    match find_entry(&project, &args.folder) {
        Some(entry) => attach_configured(&project, entry, args),
        None => attach_new(&project, args),
    }
}

/// The configured subrepo this folder names — by path, or failing that by name.
fn find_entry<'a>(project: &'a Project, folder: &str) -> Option<&'a ResolvedSubrepo> {
    let by_path = normalize_subrepo_path(folder)
        .ok()
        .and_then(|sub_path| project.subrepos.iter().find(|s| s.path == sub_path));
    by_path.or_else(|| project.subrepos.iter().find(|s| s.name == folder))
}

/// A per-subrepo refusal reported by a single-subrepo command: exit 2, message verbatim.
fn from_subrepo(failure: SubrepoFailure) -> Failure {
    Failure::error(failure.message)
}

/// A git error the TypeScript let escape uncaught: its `.message` becomes the whole text.
fn from_git(err: crate::core::git::GitError) -> Failure {
    Failure::error(git_message(&err))
}

/// Last segment of a `/`-separated path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------------------
// Already configured: first contact only. Nothing here may touch monosplice.toml.
// ---------------------------------------------------------------------------------------

fn attach_configured(
    project: &Project,
    entry: &ResolvedSubrepo,
    args: &AttachArgs,
) -> Result<(), Failure> {
    let root = project.root.as_path();
    let source = pull_source(entry);
    let folder = &args.folder;
    let retry = format!("monosplice attach {folder}");
    let config_path = project.config_path.display();

    if let Some(url) = args.url.as_deref() {
        if url != source {
            return Err(Failure::error(format!(
                "{}: {}/ is already configured to track {source} ({}), not {url}.
Nothing was changed — the config is untouched and no commit was made. Run `{retry}` to connect it to {source}, or edit {config_path} if you really mean to point {} at {url}.",
                entry.name, entry.path, entry.branch, entry.name
            )));
        }
    }
    if let Some(fork) = args.fork.as_deref() {
        return Err(Failure::error(format!(
            "{}: {}/ is already configured, so --fork has nothing to write.
Nothing was changed — the config is untouched and no commit was made. Edit {config_path}: set `remote` to {fork} and add `upstream = \"{}\"`, then run `{retry}`.",
            entry.name, entry.path, entry.remote
        )));
    }
    if let Some(name) = args.name.as_deref() {
        if name != entry.name {
            return Err(Failure::error(format!(
                "{}: {}/ is already configured under the name {}, so --name {name} would not be honoured.
Nothing was changed. Drop --name, or rename the subrepo in {config_path}.",
                entry.name, entry.path, entry.name
            )));
        }
    }
    if let Some(branch) = args.branch.as_deref() {
        if branch != entry.branch {
            return Err(Failure::error(format!(
                "{}: {}/ is already configured to track branch {}, not {branch}.
Nothing was changed. Drop --branch, or change `branch` in {config_path}.",
                entry.name, entry.path, entry.branch
            )));
        }
    }

    // Preconditions before the network: a dirty tree must be reported without side effects,
    // not after a fetch has already written a tracking ref.
    if let Some(state) = read_sequencer(root) {
        return Err(Failure::error(pull_in_progress_message(&state, None)));
    }
    if let Some(problem) = check_import_preconditions(root, entry, &retry) {
        return Err(Failure::error(problem));
    }

    let view = load_view(root, entry, SyncViewOptions::default()).map_err(from_subrepo)?;

    // check_import_preconditions already refused a monorepo with no commits.
    let head = rev_parse(root, "HEAD").unwrap_or_else(|| "HEAD".to_string());
    let has_content = has_committed_files(root, &head, entry);

    let Some(pub_head) = view.pub_head.clone() else {
        return publish_configured(root, entry, has_content, args);
    };
    if view.related {
        return Err(Failure::error(format!(
            "{}: already connected to {source} — monosplice trailers already link the two repositories, so there is nothing to attach.
Nothing was changed. Run `monosplice pull {}` to import new standalone-repo commits, `monosplice push {}` to export new monorepo commits, or `monosplice sync {}` for both.",
            entry.name, entry.name, entry.name, entry.name
        )));
    }

    let count = if has_content {
        snapshot_over_content(root, entry, &head, &pub_head, &retry, args)?
    } else {
        snapshot_into_empty_path(root, entry, &view, &pub_head, args)?
    };

    println!(
        "✓ {}: attached {source} ({}) at {} — {count} commit(s)",
        entry.name,
        entry.branch,
        short(&pub_head)
    );
    println!(
        "  {}/ and the remote are now in sync; push and pull as usual.",
        entry.path
    );
    // Triangular entries push to their own fork; probing it (or hinting at adding an
    // upstream that already exists) would only mislead.
    if entry.upstream.is_none() {
        warn_if_read_only(
            root,
            entry,
            &pub_head,
            &configured_fork_hint(project, entry),
        );
    }
    Ok(())
}

/// Outbound first contact for a configured subrepo: the remote branch does not exist yet.
fn publish_configured(
    root: &Path,
    entry: &ResolvedSubrepo,
    has_content: bool,
    args: &AttachArgs,
) -> Result<(), Failure> {
    if entry.upstream.is_some() {
        return Err(Failure::error(upstream_has_no_branch(entry)));
    }
    if !has_content {
        return Err(Failure::error(nothing_exists_yet(entry)));
    }
    if args.import_history {
        return Err(Failure::error(format!(
            "{}: --import-history replays the standalone repo's commits into {}/, but {} has no {} branch yet.
Nothing was changed. Drop --import-history to publish {}/ instead, adding --export-history to replay every monorepo commit that touched it.",
            entry.name, entry.path, entry.remote, entry.branch, entry.path
        )));
    }

    let result = first_publish(root, entry, args.export_history, || {
        confirm_first_publish(
            entry,
            &ConfirmFirstPublishOptions {
                yes: args.yes,
                ..Default::default()
            },
        )
    })
    .map_err(from_subrepo)?;
    println!(
        "✓ {}: published {}/ to {} ({}) — {}",
        entry.name,
        entry.path,
        entry.remote,
        entry.branch,
        published_how(result.export_history, result.commits)
    );
    Ok(())
}

/// The subrepo directory has no committed files: take the standalone repo wholesale.
fn snapshot_into_empty_path(
    root: &Path,
    entry: &ResolvedSubrepo,
    view: &SyncView,
    pub_head: &str,
    args: &AttachArgs,
) -> Result<usize, Failure> {
    if args.import_history {
        return replay_standalone_history(root, entry, view);
    }

    let pub_tree = tree_of(root, pub_head)?;
    apply_tree_into(root, entry, EMPTY_TREE, &pub_tree).map_err(from_git)?;
    commit_staged(root, &adopt_message(entry, pub_head)).map_err(from_git)?;
    Ok(1)
}

/// Both sides have content: either the trees already agree, or the user must choose.
fn snapshot_over_content(
    root: &Path,
    entry: &ResolvedSubrepo,
    head: &str,
    pub_head: &str,
    retry: &str,
    args: &AttachArgs,
) -> Result<usize, Failure> {
    if args.import_history {
        return Err(Failure::error(history_needs_empty_path(entry, retry)));
    }

    let mono_tree = filtered_subtree(root, head, entry)
        .map_err(|err| Failure::error(format!("{}: {err}\nNothing was changed.", entry.name)))?
        .unwrap_or_else(|| EMPTY_TREE.to_string());
    let pub_tree = tree_of(root, pub_head)?;

    if mono_tree != pub_tree && !args.theirs {
        return Err(Failure::error(trees_differ(
            root,
            entry,
            &mono_tree,
            &pub_tree,
            retry,
            "Nothing was changed.",
        )?));
    }

    if mono_tree != pub_tree {
        apply_tree_into(root, entry, &mono_tree, &pub_tree).map_err(from_git)?;
    }
    commit_staged(root, &adopt_message(entry, pub_head)).map_err(from_git)?;
    Ok(1)
}

// ---------------------------------------------------------------------------------------
// Not configured yet: write the entry, then make the same first-contact move.
// ---------------------------------------------------------------------------------------

fn attach_new(project: &Project, args: &AttachArgs) -> Result<(), Failure> {
    let root = project.root.as_path();
    let folder = &args.folder;
    let Some(url) = args.url.as_deref() else {
        return Err(Failure::error(format!(
            "{folder} is not a configured subrepo, so monosplice needs the repository URL to create the entry:
  monosplice attach {folder} <git-url>
Nothing was changed. Configured subrepos: {}",
            configured_names(project)
        )));
    };
    let retry = format!("monosplice attach {folder} {url}");

    let entry = plan(folder, url, args)?;
    if let Some(taken) = check_free_slot(
        &project.subrepos,
        &VendorEntry::from(&entry),
        &attach_hints(),
    ) {
        return Err(Failure::error(taken));
    }

    // Everything below writes something. Nothing above did.
    if let Some(state) = read_sequencer(root) {
        return Err(Failure::error(pull_in_progress_message(&state, None)));
    }
    if let Some(problem) = check_config_edit_preconditions(root, &retry, "Attaching") {
        return Err(Failure::error(problem));
    }

    // The tree, the anchor and every later sync decision come from the pull source: with
    // `--fork` that is upstream, and the fork is only ever written to by `push`.
    let source = pull_source(&entry).to_string();
    let pub_head = resolve_remote_head(root, &entry)?;
    let head = rev_parse(root, "HEAD").unwrap_or_else(|| "HEAD".to_string());
    let has_content = has_committed_files(root, &head, &entry);
    if !has_content {
        require_free_path(root, &entry, &retry)?;
    }

    let Some(pub_head) = pub_head else {
        if entry.upstream.is_some() {
            return Err(Failure::error(format!(
                "{}: upstream {source} has no {} branch, so there is nothing to attach to and no base for the fork branch.
Nothing was changed — the config is untouched and no commit was made. Check the URL, or name the right branch with `--branch <branch>`.",
                entry.name, entry.branch
            )));
        }
        if !has_content {
            return Err(Failure::error(format!(
                "{}\nNothing was changed — the config is untouched. Run `{retry}` again once either side has content.",
                nothing_exists_yet(&entry)
            )));
        }
        if args.import_history {
            return Err(Failure::error(format!(
                "{}: --import-history replays the standalone repo's commits into {}/, but {source} has no {} branch yet.
Nothing was changed — the config is untouched and no commit was made. Drop --import-history to publish {}/ instead, adding --export-history to replay every monorepo commit that touched it.",
                entry.name, entry.path, entry.branch, entry.path
            )));
        }
        return attach_and_publish(project, &entry, args);
    };

    attach_to_history(project, &entry, &head, &pub_head, has_content, &retry, args)
}

/// Turn the folder and the URL into the subrepo entry the rest of monosplice understands.
fn plan(folder: &str, url: &str, args: &AttachArgs) -> Result<ResolvedSubrepo, Failure> {
    if args.fork.as_deref() == Some(url) {
        return Err(Failure::error(format!(
            "--fork {url} is the same URL you are attaching, so there is no fork to push to.\nNothing was changed. Drop --fork, or point it at your own fork of {url}."
        )));
    }
    let sub_path = normalize_subrepo_path(folder).map_err(|err| {
        Failure::error(format!(
            "{err}\nNothing was changed. Name a directory inside this monorepo."
        ))
    })?;
    let branch = args.branch.clone().unwrap_or_else(|| "main".to_string());
    Ok(ResolvedSubrepo {
        name: args
            .name
            .clone()
            .unwrap_or_else(|| basename(&sub_path).to_string()),
        path: sub_path,
        // With a fork, `remote` is where we push and the attached URL becomes `upstream`.
        remote: args.fork.clone().unwrap_or_else(|| url.to_string()),
        upstream: args.fork.as_ref().map(|_| url.to_string()),
        push_branch: branch.clone(),
        branch,
        exclude: Vec::new(),
        rewrite_message: None,
        transform: None,
        scan: None,
    })
}

/// A path with no committed files must also be empty on disk: the tree is applied with
/// `git apply --index`, which would fail halfway over files git has never seen.
fn require_free_path(root: &Path, entry: &ResolvedSubrepo, retry: &str) -> Result<(), Failure> {
    if !root.join(&entry.path).exists() {
        return Ok(());
    }
    Err(Failure::error(format!(
        "{} already exists in {}, but has no committed files, so monosplice will not write the standalone tree over it.
Nothing was changed — the config is untouched and no commit was made. Remove it (or commit its contents), then run `{retry}` again.",
        entry.path,
        root.display()
    )))
}

/// The standalone branch head, `None` when the remote has no such branch yet.
fn resolve_remote_head(root: &Path, entry: &ResolvedSubrepo) -> Result<Option<String>, Failure> {
    let source = pull_source(entry);
    let what = if entry.upstream.is_none() {
        "remote"
    } else {
        "upstream"
    };
    ls_remote_branch(root, source, &entry.branch).map_err(|err| {
        Failure::error(format!(
            "{}: cannot reach {what} {source}\n{}\nNothing was changed — the config is untouched and no commit was made.",
            entry.name,
            git_stderr(&err)
        ))
    })
}

/// Outbound first contact: the folder has content and the remote is empty. The config entry
/// is committed on its own first, so the confirmation the publish needs can be answered
/// later with the `push` command the refusal names — without editing anything by hand.
fn attach_and_publish(
    project: &Project,
    entry: &ResolvedSubrepo,
    args: &AttachArgs,
) -> Result<(), Failure> {
    let root = project.root.as_path();
    commit_entry(
        project,
        entry,
        &format!("monosplice push {} --yes", entry.name),
    )?;
    println!(
        "✓ attached {} at {} (tracking {}#{})",
        entry.name, entry.path, entry.remote, entry.branch
    );

    let result = first_publish(root, entry, args.export_history, || {
        confirm_first_publish(
            entry,
            &ConfirmFirstPublishOptions {
                yes: args.yes,
                state_note: Some(format!(
                    "The config entry for {} was committed, but nothing was pushed.",
                    entry.name
                )),
                cancel_note: Some(format!(
                    " The config entry was committed — run `monosplice push {} --yes` when you are ready.",
                    entry.name
                )),
            },
        )
    })
    .map_err(from_subrepo)?;
    println!(
        "✓ {}: published {}/ to {} ({}) — {}",
        entry.name,
        entry.path,
        entry.remote,
        entry.branch,
        published_how(result.export_history, result.commits)
    );
    Ok(())
}

/// Inbound first contact: the remote already has history. The config entry rides along in
/// the same commit — the anchor and the entry that gives it meaning belong together. With
/// `--import-history` it cannot: each replayed commit is its own, so the entry is committed
/// first.
fn attach_to_history(
    project: &Project,
    entry: &ResolvedSubrepo,
    head: &str,
    pub_head: &str,
    has_content: bool,
    retry: &str,
    args: &AttachArgs,
) -> Result<(), Failure> {
    let root = project.root.as_path();
    let source = pull_source(entry).to_string();
    fetch_branch(
        root,
        &source,
        &entry.branch,
        &remote_tracking_ref(&entry.name),
    )
    .map_err(from_git)?;

    let pub_tree = tree_of(root, pub_head)?;
    let mono_tree = if has_content {
        filtered_subtree(root, head, entry)
            .map_err(|err| Failure::error(err.to_string()))?
            .unwrap_or_else(|| EMPTY_TREE.to_string())
    } else {
        EMPTY_TREE.to_string()
    };

    if args.import_history && has_content {
        return Err(Failure::error(history_needs_empty_path(entry, retry)));
    }

    if has_content && mono_tree != pub_tree && !args.theirs {
        return Err(Failure::error(trees_differ(
            root,
            entry,
            &mono_tree,
            &pub_tree,
            retry,
            "Nothing was changed — the config is untouched and no commit was made.",
        )?));
    }

    let mut replayed = 0;
    if args.import_history {
        commit_entry(project, entry, &format!("{retry} --import-history"))?;
        let view = load_view(root, entry, SyncViewOptions::default()).map_err(from_subrepo)?;
        replayed = replay_standalone_history(root, entry, &view)?;
    } else {
        insert_entry(project, entry, &format!("monosplice attach {}", entry.path))?;
        let config_path = project.config_path.display().to_string();
        git(root, &["add", "--", &config_path]).map_err(from_git)?;
        if mono_tree != pub_tree {
            apply_tree_into(root, entry, &mono_tree, &pub_tree).map_err(from_git)?;
        }
        commit_staged(root, &adopt_message(entry, pub_head)).map_err(from_git)?;
    }

    let how = if args.import_history {
        format!(" — replayed {replayed} commit(s)")
    } else {
        String::new()
    };
    println!(
        "✓ attached {} at {} (tracking {source}#{}) @ {}{how}",
        entry.name,
        entry.path,
        entry.branch,
        short(pub_head)
    );
    println!(
        "  {}/ and the remote are now in sync; push and pull as usual.",
        entry.path
    );
    if entry.upstream.is_some() {
        println!(
            "  `monosplice push {}` rebuilds {} ({}) as {source}'s {} plus your patches — open the PR from there.",
            entry.name, entry.remote, entry.push_branch, entry.branch
        );
        return Ok(());
    }
    warn_if_read_only(
        root,
        entry,
        pub_head,
        &format!("{retry} --fork <your-fork-url>"),
    );
    Ok(())
}

/// Write the entry, or exit non-zero leaving it on stdout so it can be copy-pasted.
fn insert_entry(
    project: &Project,
    entry: &ResolvedSubrepo,
    next_command: &str,
) -> Result<(), Failure> {
    let failure =
        write_config_entry(project, entry).map_err(|err| Failure::error(err.to_string()))?;
    let Some(failure) = failure else {
        return Ok(());
    };
    let (log, error) = paste_it_yourself(&project.config_path, &failure, next_command);
    println!("{log}");
    Err(Failure::error(error))
}

/// Write the entry and commit it on its own, so what follows starts from a clean index.
fn commit_entry(
    project: &Project,
    entry: &ResolvedSubrepo,
    next_command: &str,
) -> Result<(), Failure> {
    insert_entry(project, entry, next_command)?;
    let config_path = project.config_path.display().to_string();
    git(&project.root, &["add", "--", &config_path]).map_err(from_git)?;
    commit_staged(
        &project.root,
        &format!(
            "Attach {}: track {} ({})",
            entry.name,
            pull_source(entry),
            entry.branch
        ),
    )
    .map_err(from_git)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Shared moves and shared wording.
// ---------------------------------------------------------------------------------------

/// Replay every unreflected standalone-repo commit into the subrepo. Returns how many landed.
fn replay_standalone_history(
    root: &Path,
    entry: &ResolvedSubrepo,
    view: &SyncView,
) -> Result<usize, Failure> {
    let result = run_import(
        root,
        entry,
        &view.unreflected_pub,
        view.import_base.as_deref(),
        &mut |message| warn(&message),
        None,
    )
    .map_err(|err| from_subrepo(report_import_failure(entry, err, None)))?;
    Ok(result.imported.len())
}

/// The tree of a commit, as `rev-parse <sha>^{tree}` reports it.
fn tree_of(root: &Path, commit: &str) -> Result<String, Failure> {
    git(root, &["rev-parse", &format!("{commit}^{{tree}}")]).map_err(from_git)
}

/// How a first publish describes what it did.
fn published_how(export_history: bool, commits: usize) -> String {
    if export_history {
        format!("replayed {commits} commit(s)")
    } else {
        "one baseline commit".to_string()
    }
}

fn history_needs_empty_path(entry: &ResolvedSubrepo, retry: &str) -> String {
    format!(
        "{}: --import-history replays the standalone repo's history into an empty path, but {}/ already has committed files.
Nothing was changed. Run `{retry}` (add --theirs if the standalone tree should win).",
        entry.name, entry.path
    )
}

fn trees_differ(
    root: &Path,
    entry: &ResolvedSubrepo,
    mono_tree: &str,
    pub_tree: &str,
    retry: &str,
    state_note: &str,
) -> Result<String, Failure> {
    let paths = differing_paths(root, mono_tree, pub_tree).map_err(from_git)?;
    Ok(format!(
        "{}: {}/ and {} ({}) both have content, and their trees differ:
{}
{state_note} Either make the two trees match and run `{retry}` again, or take the standalone tree wholesale:
  {retry} --theirs",
        entry.name,
        entry.path,
        pull_source(entry),
        entry.branch,
        paths
            .iter()
            .map(|p| format!("  {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

/// Advisory only. Attaching proves you can *read* the remote; pushing needs rights this
/// command never exercised, and finding that out on the first `push` — after the anchor
/// commit is already in your history — is the worst possible moment. Never blocks, never
/// changes the exit code: a probe that cannot decide must not veto a successful attach.
fn warn_if_read_only(root: &Path, entry: &ResolvedSubrepo, pub_head: &str, fork_hint: &str) {
    let Some(refusal) = probe_push_access(root, &entry.remote, pub_head, &entry.branch) else {
        return;
    };
    let indented = refusal
        .split('\n')
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    warn(&format!(
        "warning: {}: attached, but a dry-run push to {} was refused:
{indented}
`monosplice pull {}` will still work; `monosplice push {}` will most likely fail. If you cannot push to {}, connect through a fork of it instead:
  {fork_hint}",
        entry.name, entry.remote, entry.name, entry.name, entry.remote
    ));
}

/// A configured entry cannot be re-attached with `--fork`, so name the config edit instead.
fn configured_fork_hint(project: &Project, entry: &ResolvedSubrepo) -> String {
    format!(
        "edit {}: set `remote` to your fork and add `upstream = \"{}\"`",
        project.config_path.display(),
        entry.remote
    )
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
        }
    }

    fn project() -> Project {
        Project {
            root: PathBuf::from("/repo"),
            config_path: PathBuf::from("/repo/monosplice.toml"),
            subrepos: vec![subrepo("core", "core"), subrepo("lib", "packages/lib")],
        }
    }

    fn args(folder: &str, url: Option<&str>) -> AttachArgs {
        AttachArgs {
            folder: folder.to_string(),
            url: url.map(str::to_string),
            name: None,
            branch: None,
            yes: false,
            export_history: false,
            import_history: false,
            theirs: false,
            fork: None,
        }
    }

    #[test]
    fn an_entry_is_found_by_path_first_then_by_name() {
        let project = project();
        assert_eq!(find_entry(&project, "packages/lib").unwrap().name, "lib");
        assert_eq!(find_entry(&project, "./packages/lib/").unwrap().name, "lib");
        assert_eq!(find_entry(&project, "lib").unwrap().name, "lib");
        assert_eq!(find_entry(&project, "core").unwrap().name, "core");
    }

    #[test]
    fn an_unnormalizable_or_unknown_folder_has_no_entry() {
        let project = project();
        assert!(find_entry(&project, "..").is_none());
        assert!(find_entry(&project, "/").is_none());
        assert!(find_entry(&project, "nope").is_none());
    }

    #[test]
    fn plan_defaults_the_name_to_the_last_segment_and_the_branch_to_main() {
        let entry = plan(
            "./vendor/lodash/",
            "u",
            &args("./vendor/lodash/", Some("u")),
        )
        .unwrap();
        assert_eq!(entry.name, "lodash");
        assert_eq!(entry.path, "vendor/lodash");
        assert_eq!(entry.remote, "u");
        assert_eq!(entry.upstream, None);
        assert_eq!(entry.branch, "main");
        assert_eq!(entry.push_branch, "main");
        assert!(entry.exclude.is_empty());
    }

    #[test]
    fn a_fork_becomes_the_remote_and_the_attached_url_becomes_upstream() {
        let mut a = args("vendor/lodash", Some("up"));
        a.fork = Some("fork".to_string());
        a.branch = Some("4.x".to_string());
        a.name = Some("ld".to_string());
        let entry = plan("vendor/lodash", "up", &a).unwrap();
        assert_eq!(entry.name, "ld");
        assert_eq!(entry.remote, "fork");
        assert_eq!(entry.upstream.as_deref(), Some("up"));
        assert_eq!(entry.branch, "4.x");
        assert_eq!(entry.push_branch, "4.x");
    }

    #[test]
    fn a_fork_equal_to_the_attached_url_is_refused_before_anything_else() {
        let mut a = args("vendor/lodash", Some("u"));
        a.fork = Some("u".to_string());
        let err = plan("vendor/lodash", "u", &a).expect_err("no fork to push to");
        assert!(err
            .message
            .starts_with("--fork u is the same URL you are attaching"));
        assert!(err.message.contains("Nothing was changed."));
    }

    #[test]
    fn a_path_outside_the_monorepo_names_what_to_do_instead() {
        let err = plan("..", "u", &args("..", Some("u"))).expect_err("not a directory inside");
        assert!(err
            .message
            .ends_with("\nNothing was changed. Name a directory inside this monorepo."));
    }

    #[test]
    fn the_configured_fork_hint_names_the_toml_key_to_add() {
        assert_eq!(
            configured_fork_hint(&project(), &subrepo("core", "core")),
            "edit /repo/monosplice.toml: set `remote` to your fork and add `upstream = \"git@example.test:core.git\"`"
        );
    }
}
