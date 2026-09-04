//! Port of the corresponding TypeScript module — see docs/rust-port.md.
//!
//! The per-subrepo operations `push`, `pull`, `sync`, `status` and `tag` share, and the wording
//! they all speak with. Nothing here talks to clap: a refusal is a [`SubrepoFailure`], which the
//! single-subrepo commands turn into a [`crate::report::Failure`] and the multi-subrepo walks
//! hand to [`crate::report::each_subrepo`] to be collected.
//!
//! Output model, unchanged from the TS reporter: `log` is stdout (pipeable), every notice and
//! diagnostic is stderr.

use std::io::{IsTerminal, Write};
use std::path::Path;

use crate::config::ResolvedSubrepo;
use crate::core::exporter::{
    check_export_preconditions, plan_export, publish_baseline, publish_full_history,
    recover_export_anchor, run_export, ExportError,
};
use crate::core::filter::{has_committed_files, FilterError};
use crate::core::git::{commit_subjects, rev_list, rev_parse, GitError};
use crate::core::importer::{check_import_preconditions, run_import, ImportError, PullSequencer};
use crate::core::sync_view::{
    load_sync_view, pull_source, SyncView, SyncViewError, SyncViewOptions,
};
use crate::report::{warn, SubrepoFailure};

/// The first ten characters of a sha, the length every message abbreviates to.
pub fn short(sha: &str) -> &str {
    // Char indices, not bytes: `ls-remote` output reaches here and must never panic.
    match sha.char_indices().nth(10) {
        Some((idx, _)) => &sha[..idx],
        None => sha,
    }
}

/// git's stderr as the TypeScript reported it: execa stripped exactly one final newline before
/// the text was pasted into a user-facing message, so messages here do the same rather than
/// ending in a blank line.
pub(crate) fn git_stderr(err: &GitError) -> String {
    let s = err.stderr.as_str();
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s).to_string()
}

/// A [`GitError`]'s `.message`, with the same one-newline strip as [`git_stderr`]. Rebuilt
/// rather than taken from `Display` so the trimming applies; `undefined` is what the TS
/// template literal rendered for a signal-killed git, and messages here are verbatim ports.
pub(crate) fn git_message(err: &GitError) -> String {
    let code = match err.exit_code {
        Some(c) => c.to_string(),
        None => "undefined".to_string(),
    };
    format!(
        "git {} failed (exit {})\n{}",
        err.git_args.join(" "),
        code,
        git_stderr(err)
    )
}

/// An error the TypeScript let escape uncaught: it becomes the whole message, unadorned.
fn raw_failure(err: impl std::fmt::Display) -> SubrepoFailure {
    SubrepoFailure::new(err.to_string())
}

/// The git error underneath an export failure, if that is what it is. `filtered_subtree` wraps
/// git errors in [`FilterError`]; the TypeScript threw the `GitError` itself, so the two are the
/// same case for reporting.
fn export_git_cause(err: &ExportError) -> Option<&GitError> {
    match err {
        ExportError::Git(e) => Some(e),
        ExportError::Filter(FilterError::Git(e)) => Some(e),
        _ => None,
    }
}

/// Derive the sync view, turning an unreachable source repository into a user-facing error.
pub fn load_view(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    opts: SyncViewOptions,
) -> Result<SyncView, SubrepoFailure> {
    load_sync_view(root, subrepo, &opts).map_err(|err| match err {
        SyncViewError::Git(err) => unreachable_source(subrepo, &err),
        // Only `status --offline` can produce this, and it handles the case itself before it
        // ever reaches here.
        other => SubrepoFailure::new(other.to_string()),
    })
}

/// "cannot reach ..." — the source repository is the one every sync decision is made against,
/// so in triangular mode this names upstream, never the fork.
pub fn unreachable_source(subrepo: &ResolvedSubrepo, err: &GitError) -> SubrepoFailure {
    let what = if subrepo.upstream.is_none() {
        "remote"
    } else {
        "upstream"
    };
    SubrepoFailure::new(format!(
        "{}: cannot reach {what} {}\n{}",
        subrepo.name,
        pull_source(subrepo),
        git_stderr(err)
    ))
}

/// Neither side has anything: the one matrix cell where no monosplice command can help.
pub fn nothing_exists_yet(subrepo: &ResolvedSubrepo) -> String {
    format!(
        "{}: nothing exists yet — {}/ has no committed files at HEAD, and {} has no {} branch.
Commit something under {}/ and run `monosplice push {} --yes` to publish it, or run `monosplice attach {}` once the remote has content.",
        subrepo.name,
        subrepo.path,
        pull_source(subrepo),
        subrepo.branch,
        subrepo.path,
        subrepo.name,
        subrepo.path
    )
}

/// The standalone branch has history, but nothing on either side references the other.
pub fn unrelated_remote(subrepo: &ResolvedSubrepo, consequence: &str) -> String {
    format!(
        "{}: {} ({}) has history that is unrelated to this monorepo — no commit on either side references the other.
{consequence} To connect the two repositories, run:
  monosplice attach {}",
        subrepo.name,
        pull_source(subrepo),
        subrepo.branch,
        subrepo.path
    )
}

/// Stop unless the standalone branch exists, distinguishing "not published" from "nothing at all".
pub fn require_published(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
) -> Result<(), SubrepoFailure> {
    if view.pub_head.is_some() {
        return Ok(());
    }
    if subrepo.upstream.is_some() {
        return Err(SubrepoFailure::new(upstream_has_no_branch(subrepo)));
    }
    let published = match rev_parse(root, "HEAD") {
        Some(head) => has_committed_files(root, &head, subrepo),
        None => false,
    };
    if !published {
        return Err(SubrepoFailure::new(nothing_exists_yet(subrepo)));
    }
    Err(SubrepoFailure::new(format!(
        "{}: {} has no {} branch — this subrepo has not been published yet.\nRun `monosplice push {} --yes` to publish {}/ for the first time.",
        subrepo.name, subrepo.remote, subrepo.branch, subrepo.name, subrepo.path
    )))
}

/// Triangular first contact has no sensible answer: the fork branch is built *on* the upstream
/// head, so with no upstream branch there is nothing to base it on and publishing a fork from
/// scratch would defeat the point of the triangle.
pub fn upstream_has_no_branch(subrepo: &ResolvedSubrepo) -> String {
    format!(
        "{}: upstream {} has no {} branch, so monosplice has nothing to base the fork branch on.
Nothing was changed. Fix `upstream` or `branch` in your config, or drop `upstream` to publish {}/ to {} directly:
  monosplice push {} --yes",
        subrepo.name,
        subrepo.upstream.as_deref().unwrap_or(""),
        subrepo.branch,
        subrepo.path,
        subrepo.remote,
        subrepo.name
    )
}

/// What `pull` calls itself when it offers to finish an interrupted import.
pub const PULL_CONTINUE: &str = "monosplice pull --continue";

/// The two ways out of a conflicted import, named the same way everywhere. `sync` finishes its
/// own interrupted run, so it substitutes its own verb — but abort is always `pull --abort`:
/// there is one sequencer, and throwing it away is the same act whichever command wrote it.
pub fn resolve_or_abort(continue_command: Option<&str>) -> String {
    format!(
        "  {}
To abandon the import instead, restoring the monorepo to its pre-pull state:
  monosplice pull --abort",
        continue_command.unwrap_or(PULL_CONTINUE)
    )
}

/// The two ways out, with `pull`'s wording — what every command but `sync` says.
pub fn resolve_or_abort_pull() -> String {
    resolve_or_abort(None)
}

/// `--continue` with nothing to continue, worded identically wherever it is offered.
pub const NO_PULL_IN_PROGRESS: &str =
    "No pull is in progress — nothing to continue.\nRun `monosplice pull` to import new standalone-repo commits.";

/// Shared by `pull` and `sync`: neither may start while a sequencer sits on disk.
pub fn pull_in_progress_message(state: &PullSequencer, continue_command: Option<&str>) -> String {
    format!(
        "A pull of {} is already in progress.
Nothing was changed. Resolve the conflict, `git add` the files, then run:
{}",
        state.subrepo,
        resolve_or_abort(continue_command)
    )
}

/// Turn an import failure into the refusal the user reads. A conflict wrote the sequencer, and
/// only one of those can exist, so that one halts the whole run.
pub fn report_import_failure(
    subrepo: &ResolvedSubrepo,
    err: ImportError,
    continue_command: Option<&str>,
) -> SubrepoFailure {
    match err {
        ImportError::Conflict(conflict) => SubrepoFailure::halting(format!(
            "{}: importing {} conflicts with local changes.
Conflicted files:
{}
Edit each file to resolve the markers, `git add` it, then run:
{}",
            subrepo.name,
            short(&conflict.pub_sha),
            conflict
                .conflicts
                .iter()
                .map(|f| format!("  {f}"))
                .collect::<Vec<_>>()
                .join("\n"),
            resolve_or_abort(continue_command)
        )),
        ImportError::Git(err) => {
            SubrepoFailure::new(format!("{}: {}", subrepo.name, git_message(&err)))
        }
        other => SubrepoFailure::new(format!("{}: {other}", subrepo.name)),
    }
}

/// Import every unreflected standalone-repo commit. Returns how many landed.
pub fn import_subrepo(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    continue_command: Option<&str>,
) -> Result<usize, SubrepoFailure> {
    let view = load_view(root, subrepo, SyncViewOptions::default())?;
    require_published(root, subrepo, &view)?;
    if !view.related {
        return Err(SubrepoFailure::new(unrelated_remote(
            subrepo,
            "Nothing was imported.",
        )));
    }

    let retry = format!("monosplice pull {}", subrepo.name);
    if let Some(problem) = check_import_preconditions(root, subrepo, &retry) {
        return Err(SubrepoFailure::new(problem));
    }

    let result = run_import(
        root,
        subrepo,
        &view.unreflected_pub,
        &mut |message| warn(&message),
        None,
    )
    .map_err(|err| report_import_failure(subrepo, err, continue_command))?;

    Ok(result.imported.len())
}

/// What one export run did, from the caller's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportSummary {
    /// Commits written to the remote by this run.
    pub pushed: usize,
    /// Triangular only: commits the fork branch already carries byte-for-byte, so nothing was
    /// written. They stay "to push" until upstream merges them.
    pub awaiting: usize,
}

/// Re-derive an anchor that a history rewrite moved, and say so. A rebase that left the
/// subrepo tree byte-identical published exactly the right content under a sha that no longer
/// exists; adopting the commit that carries that tree today is bookkeeping, not a new decision,
/// so it is a notice rather than a question. When nothing matches, the view is untouched and
/// the caller's precondition check refuses exactly as it always did.
fn recover_anchor(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &mut SyncView,
) -> Result<(), SubrepoFailure> {
    if let Some(recovery) = recover_export_anchor(root, subrepo, view).map_err(raw_failure)? {
        warn(&recovery.message(&subrepo.name));
    }
    Ok(())
}

/// Export every pending monorepo commit.
pub fn export_subrepo(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    loaded: Option<SyncView>,
) -> Result<ExportSummary, SubrepoFailure> {
    let mut view = match loaded {
        Some(view) => view,
        None => load_view(root, subrepo, SyncViewOptions::default())?,
    };
    require_published(root, subrepo, &view)?;
    if !view.related {
        return Err(SubrepoFailure::new(unrelated_remote(
            subrepo,
            &format!("Nothing was pushed to {}.", subrepo.remote),
        )));
    }

    recover_anchor(root, subrepo, &mut view)?;

    if let Some(unsafe_state) = check_export_preconditions(root, subrepo, &view) {
        return Err(SubrepoFailure::new(unsafe_state));
    }

    if !view.unreflected_pub.is_empty() {
        return Err(SubrepoFailure::new(format!(
            "{}: {} commit(s) on {} have not been imported yet.\nNothing was pushed. Run `monosplice pull {}` first, then push again.",
            subrepo.name,
            view.unreflected_pub.len(),
            pull_source(subrepo),
            subrepo.name
        )));
    }

    let candidates = plan_export(root, subrepo, &view).map_err(raw_failure)?;
    let result = run_export(root, subrepo, &view, &candidates).map_err(|err| {
        // Everything up to the push is local, so in triangular mode a git failure here is the
        // fork's — never upstream's, which this code path does not write to at all.
        match export_git_cause(&err) {
            Some(git_err) if subrepo.upstream.is_some() => {
                let detail = git_stderr(git_err);
                let detail = if detail.is_empty() {
                    git_message(git_err)
                } else {
                    detail
                };
                SubrepoFailure::new(format!(
                    "{}: cannot push to fork remote {} ({})\n{detail}\nNothing was pushed. Fix `remote` in your config or your network/credentials, then run `monosplice push {}` again.",
                    subrepo.name, subrepo.remote, subrepo.push_branch, subrepo.name
                ))
            }
            Some(git_err) => {
                SubrepoFailure::new(format!("{}: {}", subrepo.name, git_message(git_err)))
            }
            None => SubrepoFailure::new(format!(
                "{}: {err}\nNothing was pushed to {}.",
                subrepo.name, subrepo.remote
            )),
        }
    })?;

    Ok(if result.pushed {
        ExportSummary {
            pushed: result.exported.len(),
            awaiting: 0,
        }
    } else {
        ExportSummary {
            pushed: 0,
            awaiting: result.exported.len(),
        }
    })
}

// ---------------------------------------------------------------------------------------
// Dry runs. Everything below reads; nothing below writes an object, a ref or a file.
// ---------------------------------------------------------------------------------------

/// One line of a dry run: the two fields a human scans a plan for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCommit {
    pub sha: String,
    pub subject: String,
}

/// The marker that keeps a dry run from being mistaken for a real one.
pub const DRY_RUN_NOTE: &str = "dry run — nothing written";

/// What a `--dry-run` found for one subrepo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DryRunPlan {
    Export {
        commits: Vec<PendingCommit>,
    },
    Import {
        commits: Vec<PendingCommit>,
    },
    /// The remote branch does not exist: the real push would publish it for the first time.
    FirstPublish {
        export_history: bool,
        commits: Vec<PendingCommit>,
    },
}

impl DryRunPlan {
    pub fn commits(&self) -> &[PendingCommit] {
        match self {
            DryRunPlan::Export { commits }
            | DryRunPlan::Import { commits }
            | DryRunPlan::FirstPublish { commits, .. } => commits,
        }
    }
}

fn pending(root: &Path, shas: &[String]) -> Result<Vec<PendingCommit>, SubrepoFailure> {
    let subjects = commit_subjects(root, shas).map_err(|err| raw_failure(git_message(&err)))?;
    Ok(shas
        .iter()
        .map(|sha| PendingCommit {
            sha: sha.clone(),
            subject: subjects.get(sha).cloned().unwrap_or_default(),
        })
        .collect())
}

/// What `push` would attempt, from the same `plan_export` candidate scan `push` and `status`
/// already share.
///
/// Scan and transform hooks are deliberately NOT run: they are the gate on writing to a
/// remote, and a dry run does not write, so this reports what would be *attempted*. The
/// consequence — a candidate the tree-equality check or an exclude pattern would later drop
/// still appears — is the honest direction to err in for a preview.
pub fn plan_push_dry_run(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    export_history: bool,
) -> Result<DryRunPlan, SubrepoFailure> {
    let mut view = load_view(root, subrepo, SyncViewOptions::default())?;

    if view.pub_head.is_none() {
        if subrepo.upstream.is_some() {
            return Err(SubrepoFailure::new(upstream_has_no_branch(subrepo)));
        }
        let head = match rev_parse(root, "HEAD") {
            Some(head) if has_committed_files(root, &head, subrepo) => head,
            _ => return Err(SubrepoFailure::new(nothing_exists_yet(subrepo))),
        };
        let commits = if export_history {
            let shas = rev_list(
                root,
                &["--reverse", "--topo-order", &head, "--", &subrepo.path],
            )
            .map_err(|err| raw_failure(git_message(&err)))?;
            pending(root, &shas)?
        } else {
            Vec::new()
        };
        return Ok(DryRunPlan::FirstPublish {
            export_history,
            commits,
        });
    }

    if !view.related {
        return Err(SubrepoFailure::new(unrelated_remote(
            subrepo,
            &format!("Nothing was pushed to {}.", subrepo.remote),
        )));
    }

    // The preview has to see what the push will see, anchor recovery included, or a dry run
    // would refuse where the real command succeeds.
    recover_anchor(root, subrepo, &mut view)?;

    if let Some(unsafe_state) = check_export_preconditions(root, subrepo, &view) {
        return Err(SubrepoFailure::new(unsafe_state));
    }

    if !view.unreflected_pub.is_empty() {
        return Err(SubrepoFailure::new(format!(
            "{}: {} commit(s) on {} have not been imported yet.\nNothing was pushed. Run `monosplice pull {}` first, then push again.",
            subrepo.name,
            view.unreflected_pub.len(),
            pull_source(subrepo),
            subrepo.name
        )));
    }

    let candidates = plan_export(root, subrepo, &view).map_err(raw_failure)?;
    let shas: Vec<String> = candidates.into_iter().map(|c| c.mono_sha).collect();
    Ok(DryRunPlan::Export {
        commits: pending(root, &shas)?,
    })
}

/// What `pull` would import. The work-tree preconditions a real pull insists on are skipped
/// on purpose: they exist to protect a write, and there is none — refusing to *show* the
/// incoming commits because a file is edited would make the flag useless exactly when it helps.
pub fn plan_pull_dry_run(
    root: &Path,
    subrepo: &ResolvedSubrepo,
) -> Result<DryRunPlan, SubrepoFailure> {
    let view = load_view(root, subrepo, SyncViewOptions::default())?;
    require_published(root, subrepo, &view)?;
    if !view.related {
        return Err(SubrepoFailure::new(unrelated_remote(
            subrepo,
            "Nothing was imported.",
        )));
    }
    Ok(DryRunPlan::Import {
        commits: pending(root, &view.unreflected_pub)?,
    })
}

/// Wording that differs between the commands that can trigger a first publish.
#[derive(Debug, Clone, Default)]
pub struct ConfirmFirstPublishOptions {
    /// Skip the question entirely (`--yes`).
    pub yes: bool,
    /// Sentence describing what already happened, used in place of "Nothing was pushed." —
    /// `attach` has committed the config entry by the time it asks, and must say so.
    pub state_note: Option<String>,
    /// Extra sentence appended when the user answers no at a terminal.
    pub cancel_note: Option<String>,
}

/// Publishing to a standalone remote is irreversible, so the very first push asks. At a
/// terminal that is a prompt; anywhere else it is a refusal naming the exact command, because a
/// CI job must never publish a repository by accident.
pub fn confirm_first_publish(
    subrepo: &ResolvedSubrepo,
    opts: &ConfirmFirstPublishOptions,
) -> Result<(), SubrepoFailure> {
    if opts.yes {
        return Ok(());
    }
    let state_note = opts.state_note.as_deref().unwrap_or("Nothing was pushed.");

    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        print!(
            "{} ({}) is empty. Publish {}'s current tree as its first commit there? [y/N] ",
            subrepo.remote, subrepo.branch, subrepo.name
        );
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        let answer = answer.trim().to_ascii_lowercase();
        if answer == "y" || answer == "yes" {
            return Ok(());
        }
        return Err(SubrepoFailure::new(format!(
            "{}: cancelled — nothing was pushed to {}.{}",
            subrepo.name,
            subrepo.remote,
            opts.cancel_note.as_deref().unwrap_or("")
        )));
    }

    Err(SubrepoFailure::new(format!(
        "{}: {} has no {} branch — this would be the first publish of {}/.
{state_note} Publishing to a standalone remote cannot be undone, so monosplice asks first; there is no terminal here to ask at. Run:
  monosplice push {} --yes
Add --export-history to replay every monorepo commit that touched {}/ instead of publishing one baseline commit.",
        subrepo.name, subrepo.remote, subrepo.branch, subrepo.path, subrepo.name, subrepo.path
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstPublishResult {
    pub commits: usize,
    pub export_history: bool,
}

/// Outbound first contact. This is what `seed` used to be, now reachable only through `push`
/// so the default path for a new subrepo is one command with one question.
///
/// `confirm` is asked once the preflight checks pass and only then — a subrepo with nothing in
/// it must report that, not prompt about publishing nothing.
pub fn first_publish(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    export_history: bool,
    confirm: impl FnOnce() -> Result<(), SubrepoFailure>,
) -> Result<FirstPublishResult, SubrepoFailure> {
    let Some(head) = rev_parse(root, "HEAD") else {
        return Err(SubrepoFailure::new(format!(
            "{} has no commits yet — commit something under {}/ before publishing {}.",
            root.display(),
            subrepo.path,
            subrepo.name
        )));
    };
    if !has_committed_files(root, &head, subrepo) {
        return Err(SubrepoFailure::new(nothing_exists_yet(subrepo)));
    }

    confirm()?;

    let nothing_left = format!(
        "{}: nothing to publish from {}/ after applying exclude patterns — nothing was pushed.",
        subrepo.name, subrepo.path
    );

    if export_history {
        let result = publish_full_history(root, subrepo, &head)
            .map_err(|err| publish_failure(subrepo, &err))?;
        if result.exported.is_empty() {
            return Err(SubrepoFailure::new(nothing_left));
        }
        return Ok(FirstPublishResult {
            commits: result.exported.len(),
            export_history: true,
        });
    }

    let pub_sha =
        publish_baseline(root, subrepo, &head).map_err(|err| publish_failure(subrepo, &err))?;
    if pub_sha.is_none() {
        return Err(SubrepoFailure::new(nothing_left));
    }
    Ok(FirstPublishResult {
        commits: 1,
        export_history: false,
    })
}

/// Both first-publish paths report a failure the same way.
fn publish_failure(subrepo: &ResolvedSubrepo, err: &ExportError) -> SubrepoFailure {
    match export_git_cause(err) {
        Some(git_err) => SubrepoFailure::new(format!("{}: {}", subrepo.name, git_message(git_err))),
        None => SubrepoFailure::new(format!(
            "{}: {err}\nNothing was pushed to {}.",
            subrepo.name, subrepo.remote
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subrepo() -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: "core".to_string(),
            path: "core".to_string(),
            remote: "git@example.test:core.git".to_string(),
            upstream: None,
            branch: "main".to_string(),
            push_branch: "main".to_string(),
            exclude: Vec::new(),
            rewrite_message: None,
            transform: None,
            scan: None,
        }
    }

    fn triangular() -> ResolvedSubrepo {
        ResolvedSubrepo {
            upstream: Some("git@example.test:them/core.git".to_string()),
            remote: "git@example.test:me/core.git".to_string(),
            push_branch: "patches".to_string(),
            ..subrepo()
        }
    }

    #[test]
    fn the_two_ways_out_name_the_command_that_wrote_the_sequencer() {
        assert_eq!(
            resolve_or_abort_pull(),
            "  monosplice pull --continue\nTo abandon the import instead, restoring the monorepo to its pre-pull state:\n  monosplice pull --abort"
        );
        assert!(resolve_or_abort(Some("monosplice sync --continue"))
            .starts_with("  monosplice sync --continue\n"));
        // Abort is always pull's, whichever command wrote the sequencer.
        assert!(resolve_or_abort(Some("monosplice sync --continue"))
            .ends_with("  monosplice pull --abort"));
    }

    #[test]
    fn unreachable_names_upstream_in_triangular_mode_and_the_remote_otherwise() {
        let err = GitError {
            git_args: vec!["ls-remote".to_string()],
            exit_code: Some(128),
            stderr: "fatal: nope\n".to_string(),
        };
        assert_eq!(
            unreachable_source(&subrepo(), &err).message,
            "core: cannot reach remote git@example.test:core.git\nfatal: nope"
        );
        assert_eq!(
            unreachable_source(&triangular(), &err).message,
            "core: cannot reach upstream git@example.test:them/core.git\nfatal: nope"
        );
    }

    #[test]
    fn nothing_exists_yet_points_at_both_ways_out_of_the_empty_cell() {
        let message = nothing_exists_yet(&subrepo());
        assert!(message.contains("core: nothing exists yet — core/ has no committed files at HEAD, and git@example.test:core.git has no main branch."));
        assert!(message.contains("`monosplice push core --yes`"));
        assert!(message.contains("`monosplice attach core`"));
    }

    #[test]
    fn a_first_publish_refusal_off_a_terminal_names_the_exact_command() {
        // stdin/stdout are not TTYs under cargo test, which is the CI case this must refuse in.
        let failure = confirm_first_publish(&subrepo(), &ConfirmFirstPublishOptions::default())
            .expect_err("no terminal, so no prompt");
        assert!(failure.message.starts_with(
            "core: git@example.test:core.git has no main branch — this would be the first publish of core/.\nNothing was pushed."
        ));
        assert!(failure.message.contains("  monosplice push core --yes\n"));
        assert!(!failure.halt);
    }

    #[test]
    fn a_state_note_replaces_the_nothing_was_pushed_sentence() {
        let failure = confirm_first_publish(
            &subrepo(),
            &ConfirmFirstPublishOptions {
                yes: false,
                state_note: Some("The config entry was committed.".to_string()),
                cancel_note: None,
            },
        )
        .expect_err("no terminal, so no prompt");
        assert!(failure.message.contains(
            "The config entry was committed. Publishing to a standalone remote cannot be undone"
        ));
    }

    #[test]
    fn yes_skips_the_question_entirely() {
        assert!(confirm_first_publish(
            &subrepo(),
            &ConfirmFirstPublishOptions {
                yes: true,
                ..Default::default()
            }
        )
        .is_ok());
    }

    #[test]
    fn an_import_conflict_halts_the_walk_and_lists_the_files() {
        let failure = report_import_failure(
            &subrepo(),
            ImportError::Conflict(crate::core::importer::ImportConflict {
                subrepo_name: "core".to_string(),
                pub_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
                conflicts: vec!["core/a.ts".to_string(), "core/b.ts".to_string()],
                state_path: std::path::PathBuf::from("/repo/.git/monosplice/pull-state.json"),
            }),
            None,
        );
        assert!(failure.halt, "a written sequencer stops the whole run");
        assert_eq!(
            failure.message,
            "core: importing 0123456789 conflicts with local changes.\nConflicted files:\n  core/a.ts\n  core/b.ts\nEdit each file to resolve the markers, `git add` it, then run:\n  monosplice pull --continue\nTo abandon the import instead, restoring the monorepo to its pre-pull state:\n  monosplice pull --abort"
        );
    }

    #[test]
    fn short_shas_are_ten_characters() {
        assert_eq!(short("0123456789abcdef"), "0123456789");
        assert_eq!(short("abc"), "abc");
    }
}
