//! Port of the corresponding TypeScript module — see docs/rust-port.md.
//!
//! How far each subrepo is ahead of and behind its standalone remote. The `--json` row key set
//! is a contract (S85): every key is always present, `hookError` is the single optional
//! addition, and neither triangular mode nor `--offline` may change it — the human-only
//! annotations live in [`Note`], outside the row.

use std::path::Path;

use serde::Serialize;

use crate::config::ResolvedSubrepo;
use crate::core::exporter::{build_export_chain, compute_exports, plan_export, PlannedExport};
use crate::core::importer::read_sequencer;
use crate::core::sync_view::{load_sync_view, try_load_fork_state, SyncViewError, SyncViewOptions};
use crate::ops::{git_message, unplaced_status, unreachable_source};
use crate::report::{require_project, select_subrepos, warn, Failure, NO_SUBREPOS_CONFIGURED};

/// One row of the `--json` contract (S85). Field order is the key order `JSON.stringify`
/// emitted for the TypeScript object, spreads included — CI pipes this into jq.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubrepoStatus {
    pub name: String,
    pub path: String,
    pub remote: String,
    pub branch: String,
    pub pull_in_progress: bool,
    pub seeded: bool,
    /// Commits `push` would create. Null when the subrepo is not seeded.
    pub ahead: Option<usize>,
    /// Standalone-repo commits `pull` would import. Null when the subrepo is not seeded.
    pub behind: Option<usize>,
    pub in_sync: bool,
    /// Set when a scan/transform hook throws: `ahead` is then an upper bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_error: Option<String>,
}

/// `{"offline": true, "subrepos": [...]}` — `offline` first, and only under the flag.
#[derive(Serialize)]
struct StatusJson<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    offline: Option<bool>,
    subrepos: &'a [SubrepoStatus],
}

/// Human-only annotations, deliberately not part of [`SubrepoStatus`].
#[derive(Debug, Clone, Default)]
struct Note {
    /// The fork branch already carries every pending commit — we are waiting on upstream.
    awaiting_upstream: bool,
    /// Set when the fork could not be reached; the counts are still upstream-accurate.
    unreachable: Option<String>,
    /// `--offline` and this subrepo has never been fetched, so there is nothing to measure.
    no_fetch_yet: bool,
    /// A claim this monorepo cannot place: the line to print instead of the counts.
    unplaced: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    #[arg(
        value_name = "subrepo",
        help = "Only report this subrepo (defaults to all)"
    )]
    pub subrepo: Option<String>,

    #[arg(long, help = "Print machine-readable JSON and nothing else")]
    pub json: bool,

    #[arg(
        long,
        help = "Exit 1 unless every subrepo is fully in sync (for CI); the report itself is unchanged"
    )]
    pub check: bool,

    #[arg(
        long,
        help = "Fetch nothing: measure against the remote-tracking refs the last run left behind. A subrepo that has never been fetched is reported as such rather than guessed at."
    )]
    pub offline: bool,
}

pub fn run(args: &StatusArgs) -> Result<(), Failure> {
    let project = require_project()?;
    let root = project.root.as_path();
    let state = read_sequencer(root);

    // Once per run, on stderr: the counts below are as fresh as the last fetch and no fresher,
    // and stdout stays pipeable (S156).
    if args.offline {
        warn("offline: using last-fetched state");
    }

    let selected = select_subrepos(&project, args.subrepo.as_deref())?;
    let opts = SyncViewOptions {
        offline: args.offline,
    };
    let mut rows: Vec<SubrepoStatus> = Vec::new();
    let mut notes: Vec<(String, Note)> = Vec::new();
    for subrepo in &selected {
        let pull_in_progress = state
            .as_ref()
            .is_some_and(|state| state.subrepo == subrepo.name);
        let (row, note) = inspect(root, subrepo, pull_in_progress, opts)?;
        if let Some(note) = note {
            notes.push((row.name.clone(), note));
        }
        rows.push(row);
    }

    if args.json {
        let payload = StatusJson {
            offline: if args.offline { Some(true) } else { None },
            subrepos: &rows,
        };
        let json = serde_json::to_string(&payload)
            .map_err(|err| Failure::error(format!("cannot render status JSON: {err}")))?;
        println!("{json}");
    } else if selected.is_empty() {
        println!("{NO_SUBREPOS_CONFIGURED}");
    } else {
        for row in &rows {
            describe(row, note_for(&notes, &row.name));
        }
    }

    if args.check {
        check(&rows, &notes)?;
    }
    Ok(())
}

fn note_for<'a>(notes: &'a [(String, Note)], name: &str) -> Option<&'a Note> {
    notes.iter().find(|(n, _)| n == name).map(|(_, note)| note)
}

/// The `--check` contract: exit 1 unless everything is converged and every remote answered.
/// The report above is untouched — a machine reads the exit code, a human reads the lines.
fn check(rows: &[SubrepoStatus], notes: &[(String, Note)]) -> Result<(), Failure> {
    let mut failing: Vec<&str> = Vec::new();
    for row in rows.iter().filter(|row| !row.in_sync) {
        if !failing.contains(&row.name.as_str()) {
            failing.push(&row.name);
        }
    }
    for (name, _) in notes.iter().filter(|(_, n)| n.unreachable.is_some()) {
        if !failing.contains(&name.as_str()) {
            failing.push(name);
        }
    }
    if failing.is_empty() {
        return Ok(());
    }
    let verb = if failing.len() == 1 { "is" } else { "are" };
    Err(Failure::exit1(format!(
        "--check: {} {verb} not fully in sync.\nRun `monosplice sync` to converge, or `monosplice status` for the details.",
        failing.join(", ")
    )))
}

fn inspect(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    pull_in_progress: bool,
    opts: SyncViewOptions,
) -> Result<(SubrepoStatus, Option<Note>), Failure> {
    let unmeasured = || SubrepoStatus {
        name: subrepo.name.clone(),
        path: subrepo.path.clone(),
        remote: subrepo.remote.clone(),
        branch: subrepo.branch.clone(),
        pull_in_progress,
        seeded: false,
        ahead: None,
        behind: None,
        in_sync: false,
        hook_error: None,
    };

    let view = match load_sync_view(root, subrepo, &opts) {
        Ok(view) => view,
        // Offline with no tracking ref: "never fetched" and "no branch on the remote" look the
        // same from here, so report the gap instead of picking one.
        Err(SyncViewError::NoFetchYet { .. }) => {
            return Ok((
                unmeasured(),
                Some(Note {
                    no_fetch_yet: true,
                    ..Default::default()
                }),
            ))
        }
        Err(SyncViewError::Git(err)) => {
            return Err(Failure::error(unreachable_source(subrepo, &err).message))
        }
    };
    let Some(pub_head) = view.pub_head.clone() else {
        return Ok((unmeasured(), None));
    };

    let candidates =
        plan_export(root, subrepo, &view).map_err(|err| Failure::error(err.to_string()))?;
    // Candidates over-report: tree-equality drops pure imports and excluded-only commits.
    // A throwing hook is a push-time failure, not a reason for status to blow up.
    let mut ahead = candidates.len();
    let mut hook_error: Option<String> = None;
    let mut note: Option<Note> = None;
    match compute_exports(root, subrepo, &view, &candidates) {
        Ok(planned) => {
            ahead = planned.len();
            if subrepo.upstream.is_some() {
                match inspect_fork(root, subrepo, &pub_head, &planned, opts) {
                    Ok(fork_note) => note = Some(fork_note),
                    Err(err) => hook_error = Some(err),
                }
            }
        }
        Err(err) => hook_error = Some(err.to_string()),
    }

    let behind = view.unreflected_pub.len();
    let unplaced = unplaced_status(subrepo, &view);
    let in_sync = ahead == 0 && behind == 0 && unplaced.is_none();
    if unplaced.is_some() {
        let mut stopped = note.unwrap_or_default();
        stopped.unplaced = unplaced;
        note = Some(stopped);
    }
    Ok((
        SubrepoStatus {
            seeded: true,
            ahead: Some(ahead),
            behind: Some(behind),
            in_sync,
            hook_error,
            ..unmeasured()
        },
        note,
    ))
}

/// Has the fork branch already been built from exactly these commits? Exports are
/// sha-deterministic, so rebuilding the chain locally and comparing tips answers that
/// exactly — and tells the user their patches are waiting on a maintainer, not on them.
fn inspect_fork(
    root: &Path,
    subrepo: &ResolvedSubrepo,
    pub_head: &str,
    planned: &[PlannedExport],
    opts: SyncViewOptions,
) -> Result<Note, String> {
    let (state, error) = try_load_fork_state(root, subrepo, &opts);
    if let Some(error) = error {
        let detail = error.stderr.trim();
        let detail = if detail.is_empty() {
            git_message(&error)
        } else {
            detail.to_string()
        };
        return Ok(Note {
            unreachable: Some(detail),
            ..Default::default()
        });
    }
    let head = state.and_then(|state| state.head);
    if planned.is_empty() || head.is_none() {
        return Ok(Note::default());
    }
    let (_, tip) =
        build_export_chain(root, planned, Some(pub_head)).map_err(|err| err.to_string())?;
    Ok(Note {
        awaiting_upstream: tip == head,
        ..Default::default()
    })
}

fn describe(row: &SubrepoStatus, note: Option<&Note>) {
    if note.is_some_and(|note| note.no_fetch_yet) {
        println!("{}: no fetch yet — run without --offline first", row.name);
    } else if let Some(unplaced) = note.and_then(|note| note.unplaced.as_deref()) {
        println!("{unplaced}");
    } else if !row.seeded {
        println!(
            "{}: not published yet (run `monosplice push {} --yes`)",
            row.name, row.name
        );
    } else if row.in_sync {
        println!("{}: in sync", row.name);
    } else {
        let mut parts: Vec<String> = Vec::new();
        if row.ahead.unwrap_or(0) > 0 {
            let awaiting = if note.is_some_and(|note| note.awaiting_upstream) {
                " (awaiting upstream merge)"
            } else {
                ""
            };
            parts.push(format!("{} to push{awaiting}", row.ahead.unwrap_or(0)));
        }
        if row.behind.unwrap_or(0) > 0 {
            parts.push(format!("{} to pull", row.behind.unwrap_or(0)));
        }
        println!("{}: {}", row.name, parts.join(", "));
    }

    // The counts are the report; everything below is a diagnostic, so it goes to stderr and
    // leaves stdout pipeable (S156).
    if let Some(unreachable) = note.and_then(|note| note.unreachable.as_deref()) {
        warn(&format!(
            "  ! cannot reach fork {} — the counts above are measured against upstream.",
            row.remote
        ));
        for line in unreachable.split('\n') {
            warn(&format!("    {line}"));
        }
    }
    if row.pull_in_progress {
        warn(&format!(
            "  ! a pull of {} is unfinished — resolve the conflict, `git add` the files,",
            row.name
        ));
        warn("    then run `monosplice pull --continue`, or `monosplice pull --abort` to throw it away");
    }
    if let Some(hook_error) = &row.hook_error {
        warn(&format!("  ! {hook_error}"));
        warn(&format!(
            "    `monosplice push {}` would fail with this; the count above is an upper bound.",
            row.name
        ));
    }
}
