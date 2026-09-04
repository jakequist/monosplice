//! Port of `src/commands/doctor.ts` — see docs/rust-port.md.
//!
//! Everything above [`render`] builds a model; `render` is the only place that prints. The
//! `--json` contract (S154) is that model verbatim: every key always present, `null` standing
//! in for "does not apply here", and `problems`/`notes` carrying only the headline of each
//! finding — the detail lines are advice for a terminal, not data.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::config::ResolvedSubrepo;
use crate::core::exporter::{
    compute_exports, export_base_rewritten, find_anchor_recovery, live_export_anchor, plan_export,
};
use crate::core::filter::filtered_subtree;
use crate::core::git::{git, git_ok, rev_list};
use crate::core::importer::{read_sequencer, sequencer_path};
use crate::core::sync_view::{
    is_triangular, load_sync_view, pull_source, try_load_fork_state, SyncView, SyncViewOptions,
};
use crate::core::trailers::{ORIGIN_TRAILER, SOURCE_TRAILER};
use crate::report::{require_project, select_subrepos, Failure};

#[derive(clap::Args, Debug)]
pub struct DoctorArgs {
    #[arg(
        value_name = "subrepo",
        help = "Only check this subrepo (defaults to all)"
    )]
    pub subrepo: Option<String>,

    #[arg(long, help = "Print machine-readable JSON and nothing else")]
    pub json: bool,
}

/// A problem or a note: one headline, plus prose only a human needs.
struct Finding {
    headline: String,
    detail: Vec<String>,
}

/// One row of the `--json` contract. Field order *is* the contract: `serde_json` writes struct
/// fields in declaration order, which is the order of the TypeScript interface literal.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorSubrepo {
    name: String,
    path: String,
    remote: String,
    branch: String,
    upstream: Option<String>,
    push_branch: Option<String>,
    /// Could the pull source be reached at all? Everything below is null when it could not.
    reachable: bool,
    seeded: bool,
    pub_head: Option<String>,
    /// Triangular only: head of the branch monosplice rebuilds on the fork.
    fork_head: Option<String>,
    last_exported_mono: Option<String>,
    last_exported_pub: Option<String>,
    ahead: Option<usize>,
    behind: Option<usize>,
    problems: Vec<String>,
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PullInProgress {
    subrepo: String,
    state_path: String,
}

/// Findings about monorepo history itself, which belong to no single subrepo.
#[derive(Debug, Serialize)]
struct MonorepoFindings {
    problems: Vec<String>,
    /// Findings that do not fail the run: unresolvable import provenance a live anchor has
    /// already settled.
    notes: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DoctorReport {
    ok: bool,
    problems: usize,
    pull_in_progress: Option<PullInProgress>,
    subrepos: Vec<DoctorSubrepo>,
    monorepo: MonorepoFindings,
}

/// Accumulator for one subrepo while it is being checked. The human report lines are built
/// alongside the row so they keep the interleaved order they always had; the row's own
/// `problems` list is the count of findings, so no second copy is kept.
struct Section {
    row: DoctorSubrepo,
    lines: Vec<String>,
}

/// Doctor never takes `--offline`: a report that guessed would be worse than no report.
const OPTS: SyncViewOptions = SyncViewOptions { offline: false };

pub fn run(args: &DoctorArgs) -> Result<(), Failure> {
    let project = require_project()?;
    let root = project.root.clone();

    let pull_in_progress = match read_sequencer(&root) {
        None => None,
        Some(state) => Some(PullInProgress {
            subrepo: state.subrepo,
            state_path: sequencer_path(&root)
                .map_err(|err| Failure::error(err.to_string()))?
                .display()
                .to_string(),
        }),
    };

    let subrepos = select_subrepos(&project, args.subrepo.as_deref())?;
    let mut sections: Vec<Section> = Vec::new();
    let mut fetched_pub_shas: HashSet<String> = HashSet::new();
    let mut imported_pub_shas: HashSet<String> = HashSet::new();
    let mut origin_by_mono: HashMap<String, Vec<String>> = HashMap::new();
    let mut live_anchors: Vec<String> = Vec::new();
    let mut every_subrepo_anchored = true;

    for subrepo in &subrepos {
        let (section, checked) = check_subrepo(&root, subrepo)?;
        sections.push(section);
        let Some(checked) = checked else {
            every_subrepo_anchored = false;
            continue;
        };
        for sha in rev_list(&root, &[&checked.view.tracking_ref])
            .map_err(|err| Failure::error(err.to_string()))?
        {
            fetched_pub_shas.insert(sha);
        }
        // Every view derives these from the same HEAD walk, so the last one wins and they
        // are all equal — the TypeScript did exactly this, for exactly that reason.
        imported_pub_shas = checked.view.imported_pub_shas;
        origin_by_mono = checked.view.origin_by_mono;
        match checked.live_anchor {
            Some(anchor) => live_anchors.push(anchor),
            None => every_subrepo_anchored = false,
        }
    }

    // A live anchor proves *its* subrepo agrees with *its* remote at that commit — it says
    // nothing about another subrepo's remote. An import trailer cannot be attributed to a
    // subrepo once its sha resolves nowhere, so a fossil counts as settled only when every
    // configured subrepo has a live anchor and the commit carrying it is below all of them.
    let settled = |mono_sha: &str| {
        every_subrepo_anchored
            && !live_anchors.is_empty()
            && live_anchors
                .iter()
                .all(|anchor| git_ok(&root, &["merge-base", "--is-ancestor", mono_sha, anchor]))
    };

    // Only meaningful with every subrepo in view: a Monosplice-Origin trailer in monorepo
    // history may belong to any of the configured remotes.
    let (orphans, fossils) = if args.subrepo.is_some() {
        (Vec::new(), Vec::new())
    } else {
        classify_origins(
            &imported_pub_shas,
            &fetched_pub_shas,
            &origin_by_mono,
            oldest_anchor(&root, &live_anchors),
            settled,
        )
    };

    let problems = usize::from(pull_in_progress.is_some())
        + sections.iter().map(|s| s.row.problems.len()).sum::<usize>()
        + orphans.len();

    let report = DoctorReport {
        ok: problems == 0,
        problems,
        pull_in_progress,
        subrepos: sections.iter().map(|s| s.row.clone()).collect(),
        monorepo: MonorepoFindings {
            problems: orphans.iter().map(|f| f.headline.clone()).collect(),
            notes: fossils.iter().map(|f| f.headline.clone()).collect(),
        },
    };

    if args.json {
        let json = serde_json::to_string(&report).map_err(|err| Failure::error(err.to_string()))?;
        println!("{json}");
    } else {
        render(&report, &sections, &orphans, &fossils);
    }

    if problems == 0 {
        return Ok(());
    }
    Err(Failure::exit1(format!(
        "{problems} problem(s) found — see the report above."
    )))
}

// ---------------------------------------------------------------------------------------
// Human rendering. Everything above builds the model; this is the only place that prints.
// ---------------------------------------------------------------------------------------

fn render(report: &DoctorReport, sections: &[Section], orphans: &[Finding], fossils: &[Finding]) {
    if let Some(pull) = &report.pull_in_progress {
        println!(
            "✗ an unfinished pull of {} is recorded in {}",
            pull.subrepo, pull.state_path
        );
        println!(
            "  Resolve the conflict, `git add` the files, then run `monosplice pull --continue`."
        );
        println!("  To abandon that import instead, run `monosplice pull --abort`.");
        println!();
    }
    for section in sections {
        for line in &section.lines {
            println!("{line}");
        }
        println!();
    }
    if !orphans.is_empty() || !fossils.is_empty() {
        println!("monorepo");
        for finding in orphans {
            for line in render_finding("✗", finding) {
                println!("{line}");
            }
        }
        for finding in fossils {
            for line in render_finding("!", finding) {
                println!("{line}");
            }
        }
        println!();
    }
    if report.ok {
        println!("✓ all checks passed");
    }
}

fn render_finding(mark: &str, finding: &Finding) -> Vec<String> {
    let mut out = vec![format!("  {mark} {}", finding.headline)];
    for line in &finding.detail {
        out.push(format!("    {line}"));
    }
    out
}

// ---------------------------------------------------------------------------------------
// Checks. Each one appends to the section's model; `lines` is built alongside so the human
// report keeps the interleaved order it always had.
// ---------------------------------------------------------------------------------------

fn problem(section: &mut Section, headline: String, detail: &[&str]) {
    let finding = Finding {
        headline,
        detail: detail.iter().map(|d| (*d).to_string()).collect(),
    };
    section.row.problems.push(finding.headline.clone());
    section.lines.extend(render_finding("✗", &finding));
}

fn note(section: &mut Section, headline: String, detail: &[&str]) {
    let finding = Finding {
        headline,
        detail: detail.iter().map(|d| (*d).to_string()).collect(),
    };
    section.row.notes.push(finding.headline.clone());
    section.lines.extend(render_finding("!", &finding));
}

/// What one subrepo contributed to the monorepo-wide checks: the view it derived, and the
/// commit that currently vouches for everything below it (`None` when nothing does).
struct Checked {
    view: SyncView,
    live_anchor: Option<String>,
}

fn check_subrepo(
    root: &Path,
    subrepo: &ResolvedSubrepo,
) -> Result<(Section, Option<Checked>), Failure> {
    let triangular = is_triangular(subrepo);
    let mut section = Section {
        row: DoctorSubrepo {
            name: subrepo.name.clone(),
            path: subrepo.path.clone(),
            remote: subrepo.remote.clone(),
            branch: subrepo.branch.clone(),
            upstream: subrepo.upstream.clone(),
            push_branch: if triangular {
                Some(subrepo.push_branch.clone())
            } else {
                None
            },
            reachable: true,
            seeded: false,
            pub_head: None,
            fork_head: None,
            last_exported_mono: None,
            last_exported_pub: None,
            ahead: None,
            behind: None,
            problems: Vec::new(),
            notes: Vec::new(),
        },
        lines: Vec::new(),
    };

    section.lines.push(subrepo.name.clone());
    section
        .lines
        .push(format!("  path:          {}/", subrepo.path));
    if triangular {
        section.lines.push(format!(
            "  upstream:      {} ({})",
            pull_source(subrepo),
            subrepo.branch
        ));
        section.lines.push(format!(
            "  fork:          {} ({})",
            subrepo.remote, subrepo.push_branch
        ));
    } else {
        section.lines.push(format!(
            "  remote:        {} ({})",
            subrepo.remote, subrepo.branch
        ));
    }

    let view = match load_sync_view(root, subrepo, &OPTS) {
        Ok(view) => view,
        Err(err) => {
            section.row.reachable = false;
            // `trim_end` before splitting: git's stderr keeps its trailing newline, and the
            // TypeScript's execa message did not — an empty tail would print a bare indent.
            let message = err.to_string();
            let mut detail: Vec<&str> = message.trim_end().split('\n').collect();
            detail.push(
                "Fix the URL in your config or your network/credentials, then run `monosplice doctor` again.",
            );
            problem(
                &mut section,
                format!(
                    "cannot reach {}{}",
                    if triangular { "upstream " } else { "" },
                    pull_source(subrepo)
                ),
                &detail,
            );
            return Ok((section, None));
        }
    };

    let Some(pub_head) = view.pub_head.clone() else {
        let advice = if triangular {
            "Fix `upstream` or `branch` in your config: monosplice builds the fork branch on the upstream head.".to_string()
        } else {
            format!(
                "Run `monosplice push {} --yes` to publish it for the first time.",
                subrepo.name
            )
        };
        problem(
            &mut section,
            format!(
                "not published yet — {} has no {} branch.",
                pull_source(subrepo),
                subrepo.branch
            ),
            &[advice.as_str()],
        );
        return Ok((section, None));
    };

    section.row.seeded = true;
    section.row.pub_head = Some(pub_head.clone());
    section.lines.push(format!(
        "  {} {pub_head}",
        if triangular {
            "upstream head:"
        } else {
            "pub head:     "
        }
    ));
    if triangular {
        report_fork(&mut section, root, subrepo);
    }

    section.row.last_exported_mono = view.last_exported_mono.clone();
    if let Some(last) = &view.last_exported_mono {
        let pub_sha = view.exported_mono_to_pub.get(last).cloned();
        section.row.last_exported_pub = pub_sha.clone();
        section.lines.push(format!("  last exported: mono {last}"));
        section.lines.push(format!(
            "                 pub  {}",
            pub_sha.as_deref().unwrap_or("(unknown)")
        ));
    } else {
        section
            .lines
            .push("  last exported: (nothing yet)".to_string());
    }

    report_counts(&mut section, root, subrepo, &view)?;

    for broken in &view.broken_source_refs {
        problem(
            &mut section,
            format!(
                "standalone commit {} carries {SOURCE_TRAILER}: {}, but that monorepo commit does not exist in this clone.",
                broken.pub_sha, broken.mono_sha
            ),
            &[
                "Usually the monorepo clone is missing history (a shallow or partial clone), or `remote` points",
                "at a repository that was published from a different monorepo.",
                "Run `git fetch --unshallow` (or fix `remote` in your config); monosplice refuses to export until",
                "the mapping resolves, so nothing can be published on top of a history it cannot see.",
            ],
        );
    }

    report_superseded_anchors(&mut section, &view);

    if export_base_rewritten(root, &view) {
        report_rewritten_anchor(&mut section, root, subrepo, &view);
    }

    verify_mapping(&mut section, root, subrepo, &view);
    let live_anchor = live_export_anchor(root, subrepo, &view);
    Ok((section, Some(Checked { view, live_anchor })))
}

/// Dead `Monosplice-Source` trailers *behind* the newest resolvable anchor. One machine rebased
/// after an export, so every clone made since is missing the sha that export recorded — and no
/// clone will ever have it again. It is a fact about history, not a fault to fix: the live
/// anchor above it decides what is published, so this is a note and `doctor` still exits 0.
fn report_superseded_anchors(section: &mut Section, view: &SyncView) {
    if view.superseded_source_refs.is_empty() {
        return;
    }
    let live = view.last_exported_mono.as_deref().unwrap_or("undefined");
    let oldest_first: Vec<String> = view
        .superseded_source_refs
        .iter()
        .rev()
        .map(|dead| {
            format!(
                "standalone commit {} names {} ({SOURCE_TRAILER}), which this clone does not have.",
                dead.pub_sha, dead.mono_sha
            )
        })
        .collect();
    let mut detail: Vec<&str> = oldest_first.iter().map(String::as_str).collect();
    detail.push(
        "Monorepo history was rewritten after that export, so the sha it recorded exists nowhere any",
    );
    detail.push(
        "more. A newer standalone commit names a commit this clone has, and that anchor is what push",
    );
    detail.push("and pull are measured from; nothing below it can change the answer.");
    note(
        section,
        format!(
            "informational: {} historical anchor(s) unresolvable — superseded by live anchor at {live}.",
            view.superseded_source_refs.len()
        ),
        &detail,
    );
}

/// The anchor sha left HEAD's history. Whether that is a problem depends entirely on content:
/// if some commit still on the walk publishes exactly the tree the standalone repo carries, the
/// export was correct and `push` re-derives the anchor by itself — a note, not a problem, and
/// nothing for the user to restore. Otherwise the old refusal stands, `git reflog` and all.
fn report_rewritten_anchor(
    section: &mut Section,
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
) {
    let missing = view
        .last_exported_mono
        .clone()
        .unwrap_or_else(|| "undefined".to_string());

    let recovery = find_anchor_recovery(root, subrepo, view).ok().flatten();
    let Some(recovery) = recovery else {
        problem(
            section,
            format!("the last exported monorepo commit {missing} is no longer an ancestor of HEAD."),
            &[
                "Monorepo history was rewritten (rebase, amend or force-push) underneath it, so the export range",
                "is meaningless and `monosplice push` will refuse.",
                "Restore that commit (see `git reflog`) or re-point the branch at history that contains it.",
            ],
        );
        return;
    };

    let adopt = format!(
        "`monosplice push {}` adopts {} as the anchor and exports only the work after it.",
        subrepo.name, recovery.recovered
    );
    let mut detail = vec![
        "Monorepo history was rewritten (rebase, amend or force-push), but that commit's content is still here:",
        "the rewrite left the published subrepo tree byte-identical, so nothing was lost — only the recorded sha.",
        adopt.as_str(),
    ];
    let adjacent = format!(
        "{} adjacent commits publish that same tree; the newest is the one adopted.",
        recovery.also_matching + 1
    );
    if recovery.also_matching > 0 {
        detail.push(adjacent.as_str());
    }
    note(
        section,
        format!(
            "the last exported monorepo commit {missing} is no longer an ancestor of HEAD — anchor missing; recoverable via identical tree at {}.",
            recovery.recovered
        ),
        &detail,
    );
}

/// The fork is reported separately from upstream and never conflated with it: an unreachable
/// fork blocks `push` and nothing else, so it must not read like the sync source is broken.
fn report_fork(section: &mut Section, root: &Path, subrepo: &ResolvedSubrepo) {
    let (state, error) = try_load_fork_state(root, subrepo, &OPTS);
    if let Some(error) = error {
        let message = error.to_string();
        let mut detail: Vec<&str> = message.trim_end().split('\n').collect();
        let pulling = format!(
            "Pulling still works — it only talks to {} — but `monosplice push {}` will fail.",
            subrepo.upstream.as_deref().unwrap_or(""),
            subrepo.name
        );
        detail.push(pulling.as_str());
        detail.push(
            "Fix `remote` in your config or your network/credentials, then run `monosplice doctor` again.",
        );
        problem(
            section,
            format!("cannot reach fork remote {}", subrepo.remote),
            &detail,
        );
        return;
    }
    let head = state.and_then(|s| s.head);
    let Some(head) = head else {
        section.lines.push(format!(
            "  fork head:     (no {} branch yet)",
            subrepo.push_branch
        ));
        return;
    };
    section.row.fork_head = Some(head.clone());
    section.lines.push(format!("  fork head:     {head}"));
}

fn report_counts(
    section: &mut Section,
    root: &Path,
    subrepo: &ResolvedSubrepo,
    view: &SyncView,
) -> Result<(), Failure> {
    let candidates = plan_export(root, subrepo, view).map_err(|e| Failure::error(e.to_string()))?;
    let mut ahead = candidates.len();
    let mut hook_error: Option<String> = None;
    match compute_exports(root, subrepo, view, &candidates) {
        Ok(planned) => ahead = planned.len(),
        Err(err) => hook_error = Some(err.to_string()),
    }
    section.row.ahead = Some(ahead);
    section.row.behind = Some(view.unreflected_pub.len());
    section.lines.push(format!(
        "  to push: {ahead}, to pull: {}",
        view.unreflected_pub.len()
    ));
    if let Some(hook_error) = hook_error {
        let advice = format!(
            "`monosplice push {}` will fail until that commit is fixed or the hook is changed.",
            subrepo.name
        );
        problem(
            section,
            format!("a configured hook rejects a pending commit: {hook_error}"),
            &[advice.as_str()],
        );
    }
    Ok(())
}

/// The cursor claims a standalone commit X exported mono commit Y; check the trees agree.
fn verify_mapping(section: &mut Section, root: &Path, subrepo: &ResolvedSubrepo, view: &SyncView) {
    let Some(last) = &view.last_exported_mono else {
        return;
    };
    let Some(pub_sha) = view.exported_mono_to_pub.get(last) else {
        return;
    };

    let expected = filtered_subtree(root, last, subrepo).ok().flatten();
    let actual = git(root, &["rev-parse", &format!("{pub_sha}^{{tree}}")]).ok();
    let (Some(expected), Some(actual)) = (expected, actual) else {
        return;
    };
    if expected == actual {
        return;
    }

    let advice = format!(
        "`monosplice push {}` republishes with the current config. If nothing changed, the",
        subrepo.name
    );
    note(
        section,
        format!(
            "commit {pub_sha} does not match the subtree monosplice would export from {last} today."
        ),
        &[
            "That is expected if `exclude`, `transform` or `rewrite-message` changed since that export — the next",
            advice.as_str(),
            "standalone branch was probably rewritten.",
        ],
    );
}

/// The oldest of the live anchors, which is the one every settled fossil sits below and so the
/// weakest — therefore the honest — claim the note can make. Ancestry in one repository is a
/// total order along a line of history; anything it cannot compare keeps the first anchor.
fn oldest_anchor(root: &Path, live_anchors: &[String]) -> Option<String> {
    let mut oldest = live_anchors.first()?.clone();
    for anchor in live_anchors.iter().skip(1) {
        if git_ok(root, &["merge-base", "--is-ancestor", anchor, &oldest]) {
            oldest = anchor.clone();
        }
    }
    Some(oldest)
}

/// `Monosplice-Origin` trailers naming commits no configured remote has, split by *where* they
/// sit: problems, then settled fossils.
///
/// Re-baselining a subrepo onto a freshly created repository leaves every clone's history
/// claiming imports from the repository it used to track — shas the new remote never had and
/// never will. Below a live export anchor that is settled: the anchor proves the monorepo and
/// the current remote already agree on a state, so provenance underneath it cannot change what
/// push or pull do. Above it — or with nothing anchored at all, which is what pointing at the
/// wrong remote looks like — the claim is still unexplained and still fails the run.
///
/// `settled` decides that placement; it is injected so the rule stays testable without a repo.
/// A fossil no commit on the HEAD walk carries can never be settled: there is nothing to place.
///
/// Sorted: the TypeScript walked a `Set` in insertion order, which a Rust `HashSet` cannot
/// reproduce, so the report picks the one ordering that is stable run to run.
fn classify_origins(
    imported_pub_shas: &HashSet<String>,
    fetched_pub_shas: &HashSet<String>,
    origin_by_mono: &HashMap<String, Vec<String>>,
    live_anchor: Option<String>,
    settled: impl Fn(&str) -> bool,
) -> (Vec<Finding>, Vec<Finding>) {
    let mut orphans: Vec<&String> = imported_pub_shas
        .iter()
        .filter(|sha| !fetched_pub_shas.contains(*sha))
        .collect();
    orphans.sort();

    let mut problems: Vec<Finding> = Vec::new();
    let mut fossils: Vec<String> = Vec::new();
    for sha in orphans {
        let carriers: Vec<&String> = origin_by_mono
            .iter()
            .filter(|(_, values)| values.contains(sha))
            .map(|(mono_sha, _)| mono_sha)
            .collect();
        if !carriers.is_empty() && carriers.iter().all(|mono_sha| settled(mono_sha)) {
            fossils.push(sha.clone());
            continue;
        }
        problems.push(Finding {
            headline: format!(
                "monorepo history claims to have imported commit {sha} ({ORIGIN_TRAILER}), but no configured remote has it."
            ),
            detail: vec![
                "The standalone branch was probably rewritten (force-push) after that import, or the commit came".to_string(),
                "from a subrepo that is no longer in your config.".to_string(),
            ],
        });
    }

    let notes = if fossils.is_empty() {
        Vec::new()
    } else {
        let live = live_anchor.unwrap_or_else(|| "undefined".to_string());
        let mut detail: Vec<String> = fossils
            .iter()
            .map(|sha| format!("imported commit {sha} ({ORIGIN_TRAILER}) resolves nowhere."))
            .collect();
        detail.push(
            "Those imports came from a repository this subrepo no longer tracks — a re-baseline onto a new"
                .to_string(),
        );
        detail.push(
            "remote, or a standalone branch rewritten since. The live anchor above them proves the monorepo and"
                .to_string(),
        );
        detail.push(
            "the current remote agree, so nothing below it can change what push or pull do."
                .to_string(),
        );
        vec![Finding {
            headline: format!(
                "informational: {} historical import trailer(s) unresolvable — superseded by live anchor at {live}.",
                fossils.len()
            ),
            detail,
        }]
    };

    (problems, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> DoctorSubrepo {
        DoctorSubrepo {
            name: "core".to_string(),
            path: "core".to_string(),
            remote: "git@example.test:core.git".to_string(),
            branch: "main".to_string(),
            upstream: None,
            push_branch: None,
            reachable: true,
            seeded: true,
            pub_head: Some("abc".to_string()),
            fork_head: None,
            last_exported_mono: Some("def".to_string()),
            last_exported_pub: Some("abc".to_string()),
            ahead: Some(0),
            behind: Some(0),
            problems: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// The `--json` key set and its order are the contract (S154): CI pipes this into jq.
    #[test]
    fn the_json_row_keys_are_the_typescript_interface_in_order() {
        let json = serde_json::to_string(&row()).unwrap();
        let keys: Vec<&str> = json
            .split(",\"")
            .map(|chunk| {
                chunk
                    .trim_start_matches(['{', '"'])
                    .split('"')
                    .next()
                    .unwrap_or("")
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                "name",
                "path",
                "remote",
                "branch",
                "upstream",
                "pushBranch",
                "reachable",
                "seeded",
                "pubHead",
                "forkHead",
                "lastExportedMono",
                "lastExportedPub",
                "ahead",
                "behind",
                "problems",
                "notes",
            ]
        );
    }

    #[test]
    fn absent_values_are_json_null_not_a_missing_key() {
        let json = serde_json::to_string(&row()).unwrap();
        assert!(json.contains("\"upstream\":null"), "{json}");
        assert!(json.contains("\"pushBranch\":null"), "{json}");
        assert!(json.contains("\"forkHead\":null"), "{json}");
    }

    #[test]
    fn the_report_keys_are_the_typescript_interface_in_order() {
        let report = DoctorReport {
            ok: false,
            problems: 1,
            pull_in_progress: Some(PullInProgress {
                subrepo: "core".to_string(),
                state_path: "/repo/.git/monosplice/pull-state.json".to_string(),
            }),
            subrepos: vec![row()],
            monorepo: MonorepoFindings {
                problems: vec!["orphan".to_string()],
                notes: Vec::new(),
            },
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.starts_with("{\"ok\":false,\"problems\":1,\"pullInProgress\":{\"subrepo\":\"core\",\"statePath\":\"/repo/.git/monosplice/pull-state.json\"},\"subrepos\":[{"), "{json}");
        assert!(
            json.ends_with("],\"monorepo\":{\"problems\":[\"orphan\"],\"notes\":[]}}"),
            "{json}"
        );
    }

    #[test]
    fn no_pull_in_progress_is_json_null() {
        let report = DoctorReport {
            ok: true,
            problems: 0,
            pull_in_progress: None,
            subrepos: Vec::new(),
            monorepo: MonorepoFindings {
                problems: Vec::new(),
                notes: Vec::new(),
            },
        };
        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            "{\"ok\":true,\"problems\":0,\"pullInProgress\":null,\"subrepos\":[],\"monorepo\":{\"problems\":[],\"notes\":[]}}"
        );
    }

    #[test]
    fn findings_carry_a_two_space_mark_and_four_space_detail() {
        let finding = Finding {
            headline: "it broke".to_string(),
            detail: vec!["why".to_string(), "what next".to_string()],
        };
        assert_eq!(
            render_finding("✗", &finding),
            vec!["  ✗ it broke", "    why", "    what next"]
        );
        assert_eq!(render_finding("!", &finding)[0], "  ! it broke");
    }

    fn imports(pairs: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for (mono_sha, pub_sha) in pairs {
            map.entry((*mono_sha).to_string())
                .or_default()
                .push((*pub_sha).to_string());
        }
        map
    }

    #[test]
    fn an_origin_no_remote_has_is_an_orphan_and_a_fetched_one_is_not() {
        let imported: HashSet<String> =
            ["bbb".to_string(), "aaa".to_string()].into_iter().collect();
        let fetched: HashSet<String> = ["bbb".to_string()].into_iter().collect();
        let carriers = imports(&[("mono1", "aaa"), ("mono2", "bbb")]);
        // Nothing is anchored, so nothing is settled: today's behaviour, unchanged.
        let (problems, notes) = classify_origins(&imported, &fetched, &carriers, None, |_| false);
        assert_eq!(problems.len(), 1);
        assert_eq!(
            problems[0].headline,
            "monorepo history claims to have imported commit aaa (Monosplice-Origin), but no configured remote has it."
        );
        assert!(notes.is_empty());

        let (problems, notes) = classify_origins(
            &fetched,
            &fetched,
            &carriers,
            Some("anchor".to_string()),
            |_| true,
        );
        assert!(
            problems.is_empty() && notes.is_empty(),
            "nothing is orphaned"
        );
    }

    /// The re-baseline rule: below a live anchor an unresolvable import is history, above it
    /// the claim is still unexplained. Placement is the whole decision.
    #[test]
    fn an_orphan_below_the_live_anchor_is_a_note_and_one_above_it_is_a_problem() {
        let imported: HashSet<String> = ["old".to_string(), "newer".to_string()]
            .into_iter()
            .collect();
        let fetched: HashSet<String> = HashSet::new();
        let carriers = imports(&[("below", "old"), ("above", "newer")]);

        let (problems, notes) = classify_origins(
            &imported,
            &fetched,
            &carriers,
            Some("anchor".to_string()),
            |mono_sha| mono_sha == "below",
        );
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0].headline.contains("imported commit newer"),
            "{}",
            problems[0].headline
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].headline,
            "informational: 1 historical import trailer(s) unresolvable — superseded by live anchor at anchor."
        );
        assert!(notes[0].detail[0].contains("imported commit old"));
    }

    /// A sha no commit on the HEAD walk carries cannot be placed, so it can never be settled —
    /// otherwise "below every anchor" would be vacuously true and excuse an unattributable claim.
    #[test]
    fn an_orphan_nothing_carries_is_never_settled() {
        let imported: HashSet<String> = ["ghost".to_string()].into_iter().collect();
        let (problems, notes) = classify_origins(
            &imported,
            &HashSet::new(),
            &HashMap::new(),
            Some("anchor".to_string()),
            |_| true,
        );
        assert_eq!(problems.len(), 1);
        assert!(notes.is_empty());
    }
}
