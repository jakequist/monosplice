//! Port of `src/core/sync.ts` — every sync cursor, derived from trailers on each run.
//!
//! There is no authoritative state file (CLAUDE.md): the mapping between the monorepo and a
//! public repo lives entirely in `Monosplice-Source` / `Monosplice-Origin` trailers, and this
//! module re-derives it from scratch every time.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::{monorepo_id_of, ResolvedSubrepo, CONFIG_FILENAME, LEGACY_CONFIG_FILENAMES};
use crate::core::filter::anchor_subtree;
use crate::core::git::{
    existing_commits, fetch_branch, file_versions, git, git_with, is_shallow, ls_remote_branch,
    read_blobs, rev_list, rev_list_with_parents, rev_parse, split_lines, sync_trailers, GitError,
    GitOpts,
};
use crate::core::trailers::{writer_claim, SyncTrailer, TrailerLine};

/// Where a subrepo's public branch is mirrored inside the monorepo's object db.
pub fn remote_tracking_ref(name: &str) -> String {
    format!("refs/monosplice/{name}/remote")
}

/// Where the fork's push branch is mirrored (triangular mode only).
pub fn fork_tracking_ref(name: &str) -> String {
    format!("refs/monosplice/{name}/fork")
}

/// The repository every sync decision is made against. With `upstream` configured that is
/// upstream and only upstream: the fork is a derived artifact monosplice rebuilds, so
/// consulting it for imports or anchors would let our own exports masquerade as public
/// history.
pub fn pull_source(s: &ResolvedSubrepo) -> &str {
    s.upstream.as_deref().unwrap_or(&s.remote)
}

/// Is this subrepo pulled from one repository and pushed to another?
pub fn is_triangular(s: &ResolvedSubrepo) -> bool {
    s.upstream.is_some()
}

/// How much of the network a view may use.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncViewOptions {
    /// Skip every fetch and derive the view from the remote-tracking refs already on disk.
    pub offline: bool,
}

/// Why a view could not be derived.
#[derive(Debug)]
pub enum SyncViewError {
    /// Offline, and this subrepo has never been fetched. There is no honest answer — an
    /// absent tracking ref is indistinguishable from a remote with no branch — so the caller
    /// reports the gap instead of guessing at counts.
    NoFetchYet {
        subrepo: String,
    },
    Git(GitError),
}

impl std::fmt::Display for SyncViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncViewError::NoFetchYet { subrepo } => {
                write!(f, "{subrepo}: no fetch yet — run without --offline first")
            }
            SyncViewError::Git(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SyncViewError {}

impl From<GitError> for SyncViewError {
    fn from(e: GitError) -> Self {
        SyncViewError::Git(e)
    }
}

/// What the fork's push branch looks like right now. Triangular mode only.
#[derive(Debug, Clone)]
pub struct ForkState {
    /// Fork branch head, or `None` when the fork does not have that branch yet.
    pub head: Option<String>,
}

/// Why a `Monosplice-Source` claim this monorepo cannot place stops every command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unplaced {
    /// This clone is shallow: the named commit may simply lie beyond its boundary.
    Shallow,
    /// The claim carries this monorepo's own id: an export of ours, of a commit this clone does
    /// not have (made from a branch that was deleted or squash-merged, or from a commit that
    /// was dropped or never pushed).
    OwnId,
    /// The claim carries no id, and this monorepo has not had one since its first config, so it
    /// may have written that claim itself.
    NoId,
}

/// How a claim was shown to be another monorepo's (or nobody's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foreign {
    /// It names a commit of a standalone repository monosplice fetched — no export of ours ever
    /// names anything but a monorepo commit.
    Fetched,
    /// It carries a `Monosplice-Monorepo` id this monorepo has never had.
    OtherId,
    /// It carries no id, and every claim this monorepo ever wrote carries one.
    NoIdEver,
}

/// A standalone commit's `Monosplice-Source` claim, as reported.
#[derive(Debug, Clone)]
pub struct BrokenSourceRef {
    pub pub_sha: String,
    pub mono_sha: String,
    /// The `Monosplice-Monorepo` id written with the claim, if any.
    pub monorepo: Option<String>,
    /// For a claim that could not be placed: why.
    pub unplaced: Option<Unplaced>,
    /// For another monorepo's claim: how that was shown.
    pub foreign: Option<Foreign>,
}

#[derive(Debug, Clone)]
pub struct SyncView {
    /// Local ref mirroring the public branch.
    pub tracking_ref: String,
    /// Public branch head, or `None` when the remote branch does not exist yet.
    pub pub_head: Option<String>,
    /// monorepo sha -> public sha, derived from `Monosplice-Source` trailers in pub history.
    pub exported_mono_to_pub: HashMap<String, String>,
    /// Public shas already imported into the monorepo, from `Monosplice-Origin` trailers on HEAD.
    pub imported_pub_shas: HashSet<String>,
    /// The same trailers, still attached to the monorepo commit that carries each one — the
    /// flattening of this is [`SyncView::imported_pub_shas`]. Provenance is only judgeable in
    /// place: whether an unresolvable import sha matters depends on where in history it sits.
    pub origin_by_mono: HashMap<String, Vec<String>>,
    /// Where the export scan starts: the newest commit on the HEAD walk that is either already
    /// exported (`Monosplice-Source` names it) or anchors the monorepo to the public branch
    /// (`Monosplice-Origin` naming pub head or one of its ancestors). Export scans
    /// `export_base..HEAD`; `None` means "scan all of HEAD" (nothing published yet).
    pub export_base: Option<String>,
    /// Newest monorepo commit that pub history claims to have exported and that still exists
    /// locally. Not the scan base — its job is rewrite detection: a commit that was rebased
    /// away lives on in the reflog but is absent from the HEAD walk, so `export_base` cannot
    /// see it.
    pub last_exported_mono: Option<String>,
    /// Public commits that are neither our exports nor already reflected (oldest first).
    pub unreflected_pub: Vec<String>,
    /// `Monosplice-Source` claims this monorepo cannot place: they name commits this clone does
    /// not have, sit above everything both sides agree on, and nothing shows whose they are
    /// ([`Unplaced`] says why). They could be exports of ours, so no command acts past them.
    pub broken_source_refs: Vec<BrokenSourceRef>,
    /// Claims naming commits this clone does not have, at or below something both sides agree
    /// on (an import of ours, or a verified export of ours): history, not the mapping.
    ///
    /// A rebase on one machine rewrites the sha an earlier export recorded, and every clone made
    /// afterwards is missing it forever. Once the two sides agree on a newer point, the dead
    /// trailers below it can no longer change the answer — they are reported and never refused.
    pub superseded_source_refs: Vec<BrokenSourceRef>,
    /// Claims shown to be another monorepo's ([`Foreign`] says how). They are neither exports to
    /// skip nor anchors to validate: the commits carrying them are standalone work to import.
    pub foreign_source_refs: Vec<BrokenSourceRef>,
    /// Claims naming a commit of this monorepo on standalone commits that are not its exports:
    /// a copied or cherry-picked line on work that neither reproduces that commit nor sits on
    /// work both sides agree on. The commits carrying them are standalone work to import.
    pub copied_source_refs: Vec<BrokenSourceRef>,
    /// `Monosplice-Source` trailers on the standalone branch that are not the last sync trailer
    /// of their commit: forwarded from an earlier hop (monosplice 1.0.0 copied them). Ignored.
    pub forwarded_source_trailers: usize,
    /// The same for `Monosplice-Origin` trailers in monorepo history. Ignored.
    pub forwarded_origin_trailers: usize,
    /// Do the two repos know about each other at all? False means first contact: the public
    /// branch has history, but nothing on either side references the other, so the only safe
    /// move is `monosplice attach`.
    pub related: bool,
    /// `related` only because of claims this monorepo cannot place ([`SyncView::broken_source_refs`]):
    /// no import, no export of ours, nothing settled. Every command stops at those claims except
    /// `attach`, whose snapshot of the standalone head is itself what settles them.
    pub unplaced_only: bool,
}

/// The view of a subrepo whose public branch does not exist yet.
pub fn unpublished_view(name: &str) -> SyncView {
    SyncView {
        tracking_ref: remote_tracking_ref(name),
        pub_head: None,
        exported_mono_to_pub: HashMap::new(),
        imported_pub_shas: HashSet::new(),
        origin_by_mono: HashMap::new(),
        export_base: None,
        last_exported_mono: None,
        unreflected_pub: Vec::new(),
        broken_source_refs: Vec::new(),
        superseded_source_refs: Vec::new(),
        foreign_source_refs: Vec::new(),
        copied_source_refs: Vec::new(),
        forwarded_source_trailers: 0,
        forwarded_origin_trailers: 0,
        related: false,
        unplaced_only: false,
    }
}

/// Mirror the fork's push branch locally. `ls_remote_branch` first, exactly as
/// `load_sync_view` does, so an unreachable fork raises a GitError the caller can attribute to
/// the fork rather than a fetch failure that reads like the branch is missing.
pub fn load_fork_state(
    root: &Path,
    s: &ResolvedSubrepo,
    opts: &SyncViewOptions,
) -> Result<ForkState, GitError> {
    if opts.offline {
        return Ok(ForkState {
            head: rev_parse(root, &fork_tracking_ref(&s.name)),
        });
    }
    let Some(head) = ls_remote_branch(root, &s.remote, &s.push_branch)? else {
        return Ok(ForkState { head: None });
    };
    fetch_branch(root, &s.remote, &s.push_branch, &fork_tracking_ref(&s.name))?;
    Ok(ForkState { head: Some(head) })
}

/// Fork state for reporting: an unreachable fork is a note, not a crash.
pub fn try_load_fork_state(
    root: &Path,
    s: &ResolvedSubrepo,
    opts: &SyncViewOptions,
) -> (Option<ForkState>, Option<GitError>) {
    match load_fork_state(root, s, opts) {
        Ok(state) => (Some(state), None),
        Err(e) => (None, Some(e)),
    }
}

/// Does this monorepo commit reproduce, exactly, the public commit it claims to reflect?
/// An attach anchor commit and a clean import do; a *conflicted* import and an import of a file
/// the config excludes do not — they carry work the public branch has never seen, so they
/// cannot be an export boundary.
///
/// The comparison runs through [`anchor_subtree`], so the `scan` hook is dropped: a scan that
/// rejects already-published content (a legacy secret, a hook tightened after the attach) must
/// not veto anchor detection — a vetoed anchor collapses `export_base` to "scan all of HEAD",
/// which re-exports every ancestor of the anchor. If a `transform` cannot run, the commit
/// genuinely is not a boundary and `push` reports the failure on its own terms.
fn reflects_exactly(root: &Path, s: &ResolvedSubrepo, mono_sha: &str, pub_sha: &str) -> bool {
    let Ok(Some(mono_tree)) = anchor_subtree(root, mono_sha, s) else {
        return false;
    };
    match git(root, &["rev-parse", &format!("{pub_sha}^{{tree}}")]) {
        Ok(pub_tree) => mono_tree == pub_tree,
        Err(_) => false,
    }
}

/// Walk monorepo history from HEAD and stop at the first commit whose publishable subtree the
/// public branch already contains. Two ways to qualify: pub says it exported this commit
/// (`Monosplice-Source`), or the commit imported public work and reproduces it exactly
/// (`Monosplice-Origin`) — the second is what stops a `push` right after an `attach` from
/// replaying the monorepo's entire pre-attach history onto the newly connected repo.
///
/// One `rev-list` for the walk, then O(1) lookups: both trailer maps are already in hand and
/// `pub_ancestors` is the pub-side walk this function's caller needed anyway, so an Origin
/// candidate costs a set probe rather than a `merge-base` process.
fn find_export_anchor(
    root: &Path,
    s: &ResolvedSubrepo,
    head_walk: &[String],
    exported_mono_to_pub: &HashMap<String, String>,
    origin_by_mono: &HashMap<String, Vec<String>>,
    pub_ancestors: &HashSet<String>,
) -> Result<(Option<String>, bool), GitError> {
    if exported_mono_to_pub.is_empty() && origin_by_mono.is_empty() {
        return Ok((None, false));
    }

    let mut related = !exported_mono_to_pub.is_empty();
    for mono_sha in head_walk.iter().cloned() {
        if exported_mono_to_pub.contains_key(&mono_sha) {
            return Ok((Some(mono_sha), true));
        }
        for pub_sha in origin_by_mono
            .get(&mono_sha)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            if !pub_ancestors.contains(pub_sha) {
                continue;
            }
            related = true;
            if reflects_exactly(root, s, &mono_sha, pub_sha) {
                return Ok((Some(mono_sha), true));
            }
        }
    }
    Ok((None, related))
}

/// A commit's own claim of one kind, as written by the hop that made it: the last sync trailer on
/// it, and the `Monosplice-Monorepo` id written after that trailer, if any. Returns commit ->
/// claim, plus how many trailers of that kind were forwarded from an earlier hop (ignored).
fn writer_claims(
    trailers: &HashMap<String, Vec<TrailerLine>>,
    want_source: bool,
) -> (HashMap<String, SourceClaim>, usize) {
    let mut claims = HashMap::new();
    let mut forwarded = 0;
    for (sha, lines) in trailers {
        let of_kind = lines
            .iter()
            .filter(|l| {
                matches!(l, TrailerLine::Sync(t) if matches!(t, SyncTrailer::Source(_)) == want_source)
            })
            .count();
        let Some(claim) = writer_claim(lines) else {
            continue;
        };
        let value = match (&claim.trailer, want_source) {
            (SyncTrailer::Source(v), true) | (SyncTrailer::Origin(v), false) => v.clone(),
            _ => {
                forwarded += of_kind;
                continue;
            }
        };
        forwarded += of_kind - 1;
        claims.insert(
            sha.clone(),
            SourceClaim {
                mono: value,
                monorepo: claim.monorepo,
            },
        );
    }
    (claims, forwarded)
}

/// One standalone commit's `Monosplice-Source` claim.
#[derive(Debug, Clone)]
struct SourceClaim {
    mono: String,
    monorepo: Option<String>,
}

/// Which `Monosplice-Monorepo` ids this monorepo has ever had, and whether it has had one from
/// its very first config. Read from every version `monosplice.toml` has had on HEAD's history,
/// plus the config on disk: the file survives a rebase or a `filter-repo` of the code, so a
/// rewrite cannot make one of our own ids look like a stranger's.
struct Identity {
    ever: HashSet<String>,
    /// Every claim this monorepo ever wrote carries an id: it has an id now, every committed
    /// version of its config had one, and it never used a JavaScript-era config (which predates
    /// ids). A claim with no id is then provably not ours.
    always: bool,
}

fn monorepo_identity(root: &Path, current: Option<&str>) -> Result<Identity, GitError> {
    let mut ever: HashSet<String> = current.map(str::to_string).into_iter().collect();
    let mut always = current.is_some();
    if rev_parse(root, "HEAD").is_some() {
        let mut legacy: Vec<&str> = vec!["-1", "HEAD", "--"];
        legacy.extend(LEGACY_CONFIG_FILENAMES);
        if !rev_list(root, &legacy)?.is_empty() {
            always = false;
        }
        let versions = file_versions(root, "HEAD", CONFIG_FILENAME)?;
        for blob in read_blobs(root, &versions)? {
            match blob.and_then(|b| monorepo_id_of(&String::from_utf8_lossy(&b))) {
                Some(id) => {
                    ever.insert(id);
                }
                None => always = false,
            }
        }
    }
    Ok(Identity { ever, always })
}

/// What a claim turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// This monorepo's export: the standalone tree is exactly what the named commit publishes,
    /// or the named commit is on HEAD's history and the claim sits directly on work both sides
    /// already agree on (1.0.0's reading, which a changed `exclude` or `transform` still needs),
    /// or it names a commit this clone has off HEAD's history that is nobody else's (a rewrite,
    /// which rewrite detection then refuses or recovers).
    Ours,
    /// Names no commit here, but sits at or below something both sides agree on: history that
    /// cannot change what is published or pulled.
    Settled,
    Foreign(Foreign),
    /// Names a commit of this monorepo but is not its export: a copied or cherry-picked line on
    /// standalone work that does not sit on agreed work and does not reproduce that commit.
    Copied,
    Unplaced(Unplaced),
}

/// The claims split by what could be shown about them, plus the view fields derived from them.
#[derive(Debug, Default)]
struct Claims {
    exported_mono_to_pub: HashMap<String, String>,
    last_exported_mono: Option<String>,
    broken: Vec<BrokenSourceRef>,
    superseded: Vec<BrokenSourceRef>,
    foreign: Vec<BrokenSourceRef>,
    copied: Vec<BrokenSourceRef>,
    unreflected_pub: Vec<String>,
}

/// Sort every `Monosplice-Source` claim on the standalone branch by evidence, never by looks.
///
/// **Settled** first: every standalone commit at or below one this monorepo imported
/// (`Monosplice-Origin`), or below a claim *verified* as ours — the standalone tree equals what
/// the named commit on HEAD's history publishes today, so whatever happened below it is in that
/// commit already. Ancestry, walked through the real parents, never commit-date order.
///
/// Then each claim, oldest first (see [`Verdict`] for what each outcome means):
///
/// - names a commit on HEAD's history → ours when verified, settled, or when every parent of the
///   claiming commit is settled or ours; otherwise it is a copied line, and the commit is
///   standalone work to import like any other;
/// - names a commit of a standalone repository monosplice fetched (reachable only from
///   `refs/monosplice/*`) → another monorepo's (or nobody's): no export of ours ever names one;
/// - names another commit this clone has → ours when verified or settled; another monorepo's
///   when its id says so; otherwise ours, and rewrite detection decides, as in 1.0.0;
/// - names nothing here → settled history when settled; unplaced in a shallow clone; unplaced
///   when it carries our own id; another monorepo's when it carries an id we never had, or no id
///   while we have always had one; unplaced otherwise.
///
/// Unplaced claims stop every command that could act on them. Nothing here reads authorship,
/// dates or subjects, and no claim changes how any other claim is read.
#[allow(clippy::too_many_arguments)]
fn classify_claims(
    root: &Path,
    s: &ResolvedSubrepo,
    tracking_ref: &str,
    graph: &[(String, Vec<String>)],
    head_walk: &[String],
    source_by_pub: &HashMap<String, SourceClaim>,
    imported_pub_shas: &HashSet<String>,
) -> Result<Claims, GitError> {
    // Settled by our imports: a forged or force-pushed-away Origin value would abort the whole
    // rev-list, so only values that resolve to a commit here negate anything.
    let candidates: Vec<String> = imported_pub_shas.iter().cloned().collect();
    let imports = existing_commits(root, &candidates)?;
    let mut settled: HashSet<String> = if imports.is_empty() {
        HashSet::new()
    } else {
        // --stdin instead of argv: pub histories can carry thousands of reflected commits.
        let input: String = imports.iter().map(|sha| format!("^{sha}\n")).collect();
        let unsettled: HashSet<String> = split_lines(&git_with(
            root,
            &["rev-list", tracking_ref, "--stdin"],
            GitOpts {
                input: Some(input.as_bytes()),
                ..Default::default()
            },
        )?)
        .into_iter()
        .collect();
        graph
            .iter()
            .map(|(sha, _)| sha)
            .filter(|sha| !unsettled.contains(*sha))
            .cloned()
            .collect()
    };

    let mut claimed: Vec<String> = source_by_pub.values().map(|c| c.mono.clone()).collect();
    claimed.sort();
    claimed.dedup();
    let present: HashSet<String> = existing_commits(root, &claimed)?.into_iter().collect();
    let on_head: HashSet<&str> = head_walk.iter().map(String::as_str).collect();
    let off_head = present.iter().any(|sha| !on_head.contains(sha.as_str()));
    let fetched: HashSet<String> = if !off_head {
        HashSet::new()
    } else if head_walk.is_empty() {
        rev_list(root, &["--glob=refs/monosplice"])?
            .into_iter()
            .collect()
    } else {
        rev_list(root, &["--glob=refs/monosplice", "--not", "HEAD"])?
            .into_iter()
            .collect()
    };

    // Verified exports, newest first. One on HEAD's history settles everything below it, so the
    // claims under it need no tree of their own.
    let mut verified: HashSet<&str> = HashSet::new();
    for (pub_sha, _) in graph {
        let Some(claim) = source_by_pub.get(pub_sha) else {
            continue;
        };
        if !present.contains(&claim.mono)
            || fetched.contains(&claim.mono)
            || settled.contains(pub_sha)
        {
            continue;
        }
        if !reflects_exactly(root, s, &claim.mono, pub_sha) {
            continue;
        }
        verified.insert(pub_sha);
        if on_head.contains(claim.mono.as_str()) {
            settled.extend(rev_list(root, &[pub_sha])?);
        }
    }

    let shallow = is_shallow(root);
    let mut identity: Option<Identity> = None;
    let mut foreign_by_id = |claim: &SourceClaim| -> Result<Option<Foreign>, GitError> {
        if shallow {
            return Ok(None);
        }
        if identity.is_none() {
            identity = Some(monorepo_identity(root, s.monorepo_id.as_deref())?);
        }
        let Some(identity) = &identity else {
            return Ok(None);
        };
        Ok(match &claim.monorepo {
            Some(id) if identity.ever.contains(id) => None,
            Some(_) => Some(Foreign::OtherId),
            None if identity.always => Some(Foreign::NoIdEver),
            None => None,
        })
    };

    // Oldest first: a claim that rests on its parents needs their verdicts.
    let mut verdicts: HashMap<&str, Verdict> = HashMap::new();
    let mut ours_pub: HashSet<&str> = HashSet::new();
    for (pub_sha, parents) in graph.iter().rev() {
        let Some(claim) = source_by_pub.get(pub_sha) else {
            continue;
        };
        let is_settled = settled.contains(pub_sha);
        let verdict = if !present.contains(&claim.mono) {
            if is_settled {
                match foreign_by_id(claim)? {
                    Some(why) => Verdict::Foreign(why),
                    None => Verdict::Settled,
                }
            } else if shallow {
                Verdict::Unplaced(Unplaced::Shallow)
            } else {
                match foreign_by_id(claim)? {
                    Some(why) => Verdict::Foreign(why),
                    None if claim.monorepo.is_some() => Verdict::Unplaced(Unplaced::OwnId),
                    None => Verdict::Unplaced(Unplaced::NoId),
                }
            }
        } else if on_head.contains(claim.mono.as_str()) {
            let on_agreed_work = parents
                .iter()
                .all(|p| settled.contains(p) || ours_pub.contains(p.as_str()));
            if verified.contains(pub_sha.as_str()) || is_settled || on_agreed_work {
                Verdict::Ours
            } else {
                Verdict::Copied
            }
        } else if fetched.contains(&claim.mono) {
            Verdict::Foreign(Foreign::Fetched)
        } else if verified.contains(pub_sha.as_str()) || is_settled {
            Verdict::Ours
        } else {
            match foreign_by_id(claim)? {
                Some(why) => Verdict::Foreign(why),
                None => Verdict::Ours,
            }
        };
        if verdict == Verdict::Ours {
            ours_pub.insert(pub_sha);
        }
        verdicts.insert(pub_sha, verdict);
    }

    let mut out = Claims::default();
    for (pub_sha, _) in graph {
        let (Some(claim), Some(verdict)) =
            (source_by_pub.get(pub_sha), verdicts.get(pub_sha.as_str()))
        else {
            continue;
        };
        let reference = |unplaced, foreign| BrokenSourceRef {
            pub_sha: pub_sha.clone(),
            mono_sha: claim.mono.clone(),
            monorepo: claim.monorepo.clone(),
            unplaced,
            foreign,
        };
        match *verdict {
            Verdict::Ours | Verdict::Settled | Verdict::Unplaced(_) => {
                out.exported_mono_to_pub
                    .entry(claim.mono.clone())
                    .or_insert_with(|| pub_sha.clone());
            }
            Verdict::Foreign(_) | Verdict::Copied => {}
        }
        match *verdict {
            Verdict::Ours => {
                if out.last_exported_mono.is_none() {
                    out.last_exported_mono = Some(claim.mono.clone());
                }
            }
            Verdict::Settled => out.superseded.push(reference(None, None)),
            Verdict::Unplaced(why) => out.broken.push(reference(Some(why), None)),
            Verdict::Foreign(why) => out.foreign.push(reference(None, Some(why))),
            Verdict::Copied => out.copied.push(reference(None, None)),
        }
    }

    // Standalone commits this monorepo has not seen: not settled, and not carrying a claim that
    // is ours or might be. Another monorepo's commits and copied lines are ordinary work.
    out.unreflected_pub = graph
        .iter()
        .rev()
        .filter(|(sha, _)| !settled.contains(sha))
        .filter(|(sha, _)| {
            !matches!(
                verdicts.get(sha.as_str()),
                Some(Verdict::Ours | Verdict::Settled | Verdict::Unplaced(_))
            )
        })
        .map(|(sha, _)| sha.clone())
        .collect();
    Ok(out)
}

/// Derive every sync cursor from trailers. There is no state file: this runs on each
/// invocation. `ls_remote_branch` goes first so an unreachable remote fails with a GitError
/// carrying git's own stderr, and a missing branch is reported as "not published yet" rather
/// than as a confusing fetch failure.
pub fn load_sync_view(
    root: &Path,
    s: &ResolvedSubrepo,
    opts: &SyncViewOptions,
) -> Result<SyncView, SyncViewError> {
    let tracking_ref = remote_tracking_ref(&s.name);
    let source = pull_source(s);
    let pub_head = if opts.offline {
        rev_parse(root, &tracking_ref)
    } else {
        ls_remote_branch(root, source, &s.branch)?
    };

    let head_exists = rev_parse(root, "HEAD").is_some();
    let mono_trailers = if head_exists {
        sync_trailers(root, &["HEAD"])?
    } else {
        HashMap::new()
    };
    let (writer_origins, forwarded_origin_trailers) = writer_claims(&mono_trailers, false);
    let origin_by_mono: HashMap<String, Vec<String>> = writer_origins
        .into_iter()
        .map(|(mono_sha, claim)| (mono_sha, vec![claim.mono]))
        .collect();
    let imported_pub_shas: HashSet<String> = origin_by_mono.values().flatten().cloned().collect();

    let Some(pub_head) = pub_head else {
        if opts.offline {
            return Err(SyncViewError::NoFetchYet {
                subrepo: s.name.clone(),
            });
        }
        return Ok(SyncView {
            imported_pub_shas,
            origin_by_mono,
            forwarded_origin_trailers,
            ..unpublished_view(&s.name)
        });
    };

    if !opts.offline {
        fetch_branch(root, source, &s.branch, &tracking_ref)?;
    }

    let (source_by_pub, forwarded_source_trailers) =
        writer_claims(&sync_trailers(root, &[&tracking_ref])?, true);

    let graph = rev_list_with_parents(root, &tracking_ref)?;
    let pub_ancestors: HashSet<String> = graph.iter().map(|(sha, _)| sha.clone()).collect();
    let head_walk = if head_exists && !(source_by_pub.is_empty() && origin_by_mono.is_empty()) {
        rev_list(root, &["HEAD"])?
    } else {
        Vec::new()
    };

    let claims = classify_claims(
        root,
        s,
        &tracking_ref,
        &graph,
        &head_walk,
        &source_by_pub,
        &imported_pub_shas,
    )?;

    let (export_base, related) = find_export_anchor(
        root,
        s,
        &head_walk,
        &claims.exported_mono_to_pub,
        &origin_by_mono,
        &pub_ancestors,
    )?;
    // Related through nothing but claims nobody can place: whether this is first contact is
    // exactly what cannot be told, so `attach` may make it (its snapshot settles them all).
    let unplaced_only = related
        && export_base.is_none()
        && claims.last_exported_mono.is_none()
        && claims.superseded.is_empty()
        && !claims.broken.is_empty()
        && !origin_by_mono
            .values()
            .flatten()
            .any(|pub_sha| pub_ancestors.contains(pub_sha));

    Ok(SyncView {
        tracking_ref,
        pub_head: Some(pub_head),
        exported_mono_to_pub: claims.exported_mono_to_pub,
        imported_pub_shas,
        origin_by_mono,
        export_base,
        last_exported_mono: claims.last_exported_mono,
        unreflected_pub: claims.unreflected_pub,
        broken_source_refs: claims.broken,
        superseded_source_refs: claims.superseded,
        foreign_source_refs: claims.foreign,
        copied_source_refs: claims.copied,
        forwarded_source_trailers,
        forwarded_origin_trailers,
        related,
        unplaced_only,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn subrepo(upstream: Option<&str>) -> ResolvedSubrepo {
        ResolvedSubrepo {
            name: "core".to_string(),
            path: "core".to_string(),
            remote: "fork".to_string(),
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
    fn tracking_refs_are_namespaced_per_subrepo() {
        assert_eq!(remote_tracking_ref("core"), "refs/monosplice/core/remote");
        assert_eq!(fork_tracking_ref("core"), "refs/monosplice/core/fork");
    }

    #[test]
    fn upstream_decides_where_the_tree_comes_from() {
        assert_eq!(pull_source(&subrepo(None)), "fork");
        assert!(!is_triangular(&subrepo(None)));
        assert_eq!(pull_source(&subrepo(Some("up"))), "up");
        assert!(is_triangular(&subrepo(Some("up"))));
    }

    #[test]
    fn an_unpublished_view_knows_nothing_and_is_unrelated() {
        let view = unpublished_view("core");
        assert_eq!(view.tracking_ref, "refs/monosplice/core/remote");
        assert_eq!(view.pub_head, None);
        assert!(view.exported_mono_to_pub.is_empty());
        assert!(view.imported_pub_shas.is_empty());
        assert_eq!(view.export_base, None);
        assert_eq!(view.last_exported_mono, None);
        assert!(view.unreflected_pub.is_empty());
        assert!(view.broken_source_refs.is_empty());
        assert!(!view.related);
    }

    #[test]
    fn no_fetch_yet_reads_like_the_ts_error() {
        let err = SyncViewError::NoFetchYet {
            subrepo: "core".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "core: no fetch yet — run without --offline first"
        );
    }

    // --- fixtures ---

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn hermetic() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        });
    }

    /// A monorepo with `core/`, plus a bare "public" remote for it.
    struct Fixture {
        dir: PathBuf,
        mono: PathBuf,
        remote: PathBuf,
        dates: AtomicU64,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            hermetic();
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "monosplice-syncview-{tag}-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create fixture dir");
            let f = Fixture {
                mono: dir.join("mono"),
                remote: dir.join("pub.git"),
                dir,
                dates: AtomicU64::new(0),
            };
            fs::create_dir_all(&f.mono).unwrap();
            sh(
                &f.dir,
                &format!("git init -q --bare {}", f.remote.display()),
                0,
            );
            f.sh("git init -q -b main .");
            f.sh("git config user.name 'Mono Author' && git config user.email mono@example.test");
            f.sh("mkdir -p core && printf 'hello\n' > core/README.md && printf 'top\n' > top.txt");
            f.commit("first commit");
            f
        }

        fn root(&self) -> &Path {
            &self.mono
        }

        fn remote_url(&self) -> String {
            self.remote.display().to_string()
        }

        fn subrepo(&self) -> ResolvedSubrepo {
            let mut s = subrepo(None);
            s.remote = self.remote_url();
            s
        }

        fn next_date(&self) -> u64 {
            1_767_225_600 + (self.dates.fetch_add(1, Ordering::SeqCst) + 1) * 61
        }

        fn sh(&self, cmd: &str) -> String {
            sh(&self.mono, cmd, self.next_date())
        }

        fn commit(&self, message: &str) -> String {
            self.sh(&format!(
                "git add -A && git commit -q --allow-empty -m {}",
                shq(message)
            ));
            self.sh("git rev-parse HEAD")
        }

        /// A public commit built with plumbing (no clone, no working tree), pushed to the
        /// bare remote's branch.
        fn push_pub(&self, tree: &str, parent: Option<&str>, message: &str) -> String {
            let parent = match parent {
                Some(p) => format!("-p {p}"),
                None => String::new(),
            };
            let sha = self.sh(&format!(
                "printf %s {} | git commit-tree {tree} {parent}",
                shq(message)
            ));
            self.sh(&format!(
                "git push -q --force {} {sha}:refs/heads/main",
                self.remote_url()
            ));
            sha
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn shq(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }

    fn sh(cwd: &Path, cmd: &str, date: u64) -> String {
        let stamp = format!("{date} +0000");
        let out = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .env("GIT_AUTHOR_DATE", &stamp)
            .env("GIT_COMMITTER_DATE", &stamp)
            .output()
            .expect("spawn sh");
        assert!(
            out.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn online() -> SyncViewOptions {
        SyncViewOptions { offline: false }
    }

    // --- load_sync_view ---

    #[test]
    fn an_unpublished_remote_yields_the_unpublished_view() {
        let f = Fixture::new("unpublished");
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.pub_head, None);
        assert!(!view.related);
        assert_eq!(view.export_base, None);
        assert!(view.unreflected_pub.is_empty());
    }

    #[test]
    fn offline_without_a_tracking_ref_refuses_to_guess() {
        let f = Fixture::new("offline-nofetch");
        let err = load_sync_view(f.root(), &f.subrepo(), &SyncViewOptions { offline: true })
            .expect_err("no fetch yet");
        assert!(matches!(err, SyncViewError::NoFetchYet { .. }));
        assert_eq!(
            err.to_string(),
            "core: no fetch yet — run without --offline first"
        );
    }

    #[test]
    fn a_source_trailer_derives_the_map_the_base_and_the_last_export() {
        let f = Fixture::new("source-anchor");
        let mono = f.sh("git rev-parse HEAD");
        let tree = f.sh("git rev-parse HEAD:core");
        let pub_sha = f.push_pub(
            &tree,
            None,
            &format!("first commit\n\nMonosplice-Source: {mono}\n"),
        );

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.pub_head.as_deref(), Some(pub_sha.as_str()));
        assert_eq!(
            view.exported_mono_to_pub.get(&mono).map(String::as_str),
            Some(pub_sha.as_str())
        );
        assert_eq!(view.export_base.as_deref(), Some(mono.as_str()));
        assert_eq!(view.last_exported_mono.as_deref(), Some(mono.as_str()));
        assert!(view.related);
        // Our own export is not something to pull back in.
        assert!(view.unreflected_pub.is_empty());
        assert!(view.broken_source_refs.is_empty());

        // The tracking ref is now on disk, so the offline view agrees.
        let offline = load_sync_view(f.root(), &f.subrepo(), &SyncViewOptions { offline: true })
            .expect("offline view");
        assert_eq!(offline.pub_head, view.pub_head);
        assert_eq!(offline.export_base, view.export_base);
    }

    #[test]
    fn an_origin_trailer_anchors_only_when_the_tree_matches() {
        let f = Fixture::new("origin-anchor");
        // A public commit with content the monorepo will import.
        let blob = f.sh("printf 'from pub\n' | git hash-object -w --stdin");
        let pub_tree = f.sh(&format!(
            "printf '100644 blob {blob}\\tREADME.md\\n' | git mktree"
        ));
        let pub_sha = f.push_pub(&pub_tree, None, "public work\n");

        // A *clean* import: core/ ends up byte-identical to the pub tree.
        f.sh("printf 'from pub\n' > core/README.md");
        let mono = f.commit(&format!("import\n\nMonosplice-Origin: {pub_sha}\n"));
        assert_eq!(f.sh("git rev-parse HEAD:core"), pub_tree);

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(view.related);
        assert_eq!(view.export_base.as_deref(), Some(mono.as_str()));
        assert!(view.imported_pub_shas.contains(&pub_sha));
        // Reflected by ancestry, so nothing is pending.
        assert!(view.unreflected_pub.is_empty());
        // No Source trailer anywhere: nothing was ever exported.
        assert_eq!(view.last_exported_mono, None);

        // Now break the tree equality: a *conflicted* import carries work pub has never
        // seen, so it must not become the export boundary.
        f.sh("printf 'resolved differently\n' > core/README.md");
        let conflicted = f.commit(&format!(
            "conflicted import\n\nMonosplice-Origin: {pub_sha}\n"
        ));
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(view.related);
        // The walk skips the conflicted commit and lands on the clean one below it.
        assert_eq!(view.export_base.as_deref(), Some(mono.as_str()));
        assert_ne!(view.export_base.as_deref(), Some(conflicted.as_str()));
    }

    #[test]
    fn an_origin_trailer_that_never_matches_leaves_no_base_but_still_relates() {
        let f = Fixture::new("origin-no-match");
        let blob = f.sh("printf 'from pub\n' | git hash-object -w --stdin");
        let pub_tree = f.sh(&format!(
            "printf '100644 blob {blob}\\tREADME.md\\n' | git mktree"
        ));
        let pub_sha = f.push_pub(&pub_tree, None, "public work\n");

        f.sh("printf 'never matches\n' > core/README.md");
        f.commit(&format!("bad import\n\nMonosplice-Origin: {pub_sha}\n"));

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        // Named a real pub ancestor, so the repos know about each other...
        assert!(view.related);
        // ...but no commit reproduces pub, so there is no boundary to append after.
        assert_eq!(view.export_base, None);
    }

    /// A claim naming a commit this clone does not have, with nothing to say whose it is: no
    /// `Monosplice-Monorepo` id, and this monorepo has no id of its own. It could be an export of
    /// ours that this clone cannot see, so it is a broken mapping, exactly as in 1.0.0.
    #[test]
    fn a_pub_commit_naming_an_unknown_mono_sha_is_a_broken_source_ref() {
        let f = Fixture::new("broken-source");
        let tree = f.sh("git rev-parse HEAD:core");
        let bogus = "0".repeat(40);
        let pub_sha = f.push_pub(
            &tree,
            None,
            &format!("export\n\nMonosplice-Source: {bogus}\n"),
        );

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.broken_source_refs.len(), 1);
        assert_eq!(view.broken_source_refs[0].pub_sha, pub_sha);
        assert_eq!(view.broken_source_refs[0].mono_sha, bogus);
        // A broken claim never becomes the rewrite-detection cursor.
        assert_eq!(view.last_exported_mono, None);
        // ...and it still counts as "related": pub is talking about us.
        assert!(view.related);
        // Nothing resolves anywhere, so nothing supersedes it either.
        assert!(view.superseded_source_refs.is_empty());
        assert!(view.foreign_source_refs.is_empty());
        // Neither something to import nor something to skip silently.
        assert!(view.unreflected_pub.is_empty());
    }

    /// The same branch seen from a shallow clone: a missing commit may simply be beyond the
    /// shallow boundary, so the claim could be ours and the old refusal stands.
    #[test]
    fn in_a_shallow_clone_an_unplaceable_claim_is_a_broken_source_ref() {
        let f = Fixture::new("shallow-source");
        let tree = f.sh("git rev-parse HEAD:core");
        let bogus = "0".repeat(40);
        let pub_sha = f.push_pub(
            &tree,
            None,
            &format!("export\n\nMonosplice-Source: {bogus}\n"),
        );
        f.commit("second commit");
        let shallow = f.dir.join("shallow");
        sh(
            &f.dir,
            &format!(
                "git clone -q --depth 1 file://{} {}",
                f.mono.display(),
                shallow.display()
            ),
            f.next_date(),
        );

        let view = load_sync_view(&shallow, &f.subrepo(), &online()).expect("view");
        assert_eq!(view.broken_source_refs.len(), 1);
        assert_eq!(view.broken_source_refs[0].pub_sha, pub_sha);
        assert!(view.foreign_source_refs.is_empty());
        assert_eq!(view.last_exported_mono, None);
        assert!(view.related);
        assert!(view.unreflected_pub.is_empty());
    }

    fn with_id(mut s: ResolvedSubrepo, id: &str) -> ResolvedSubrepo {
        s.monorepo_id = Some(id.to_string());
        s
    }

    /// An id this monorepo has never had makes a claim another monorepo's; its own id makes an
    /// unresolvable claim an export of its own that this clone cannot see. Each claim is read on
    /// its own: the foreign one does not change how ours is read, nor the other way round.
    #[test]
    fn a_claims_id_says_whose_it_is_and_no_claim_changes_how_another_is_read() {
        let f = Fixture::new("claim-ids");
        f.sh("printf 'id = \"ours\"\n' > monosplice.toml");
        let head = f.commit("config");
        let s = with_id(f.subrepo(), "ours");
        let tree = f.sh("git rev-parse HEAD:core");
        let e0 = f.push_pub(
            &tree,
            None,
            &format!("config\n\nMonosplice-Source: {head}\nMonosplice-Monorepo: ours\n"),
        );
        let theirs = f.push_pub(
            &tree,
            Some(&e0),
            &format!(
                "their release\n\nMonosplice-Source: {}\nMonosplice-Monorepo: theirs\n",
                "1".repeat(40)
            ),
        );
        let view = load_sync_view(f.root(), &s, &online()).expect("view");
        assert!(view.broken_source_refs.is_empty(), "{view:?}");
        assert_eq!(view.foreign_source_refs.len(), 1);
        assert_eq!(view.foreign_source_refs[0].pub_sha, theirs);
        assert_eq!(view.foreign_source_refs[0].foreign, Some(Foreign::OtherId));
        assert_eq!(view.unreflected_pub, vec![theirs.clone()]);
        assert_eq!(view.last_exported_mono.as_deref(), Some(head.as_str()));

        let lost = f.push_pub(
            &tree,
            Some(&theirs),
            &format!(
                "ours, lost\n\nMonosplice-Source: {}\nMonosplice-Monorepo: ours\n",
                "2".repeat(40)
            ),
        );
        let view = load_sync_view(f.root(), &s, &online()).expect("view");
        assert_eq!(view.broken_source_refs.len(), 1, "{view:?}");
        assert_eq!(view.broken_source_refs[0].pub_sha, lost);
        assert_eq!(view.broken_source_refs[0].unplaced, Some(Unplaced::OwnId));
        assert_eq!(view.foreign_source_refs.len(), 1, "still theirs");
        assert_eq!(view.unreflected_pub, vec![theirs]);
    }

    /// A claim with no id is provably not ours only when every claim we ever wrote carries one:
    /// an id in every version of our config. A monorepo that added its id later may have written
    /// the id-less claim itself.
    #[test]
    fn an_id_less_claim_is_foreign_only_if_we_have_always_had_an_id() {
        for (history, expect_foreign) in [(&["ours"][..], true), (&["", "ours"][..], false)] {
            let f = Fixture::new("id-less");
            for id in history {
                let line = if id.is_empty() {
                    "# no id yet".to_string()
                } else {
                    format!("id = \"{id}\"")
                };
                f.sh(&format!("printf '%s\\n' {} > monosplice.toml", shq(&line)));
                f.commit("config");
            }
            let s = with_id(f.subrepo(), "ours");
            let tree = f.sh("git rev-parse HEAD:core");
            let pub_sha = f.push_pub(
                &tree,
                None,
                &format!("a 1.0.0 export\n\nMonosplice-Source: {}\n", "3".repeat(40)),
            );
            let view = load_sync_view(f.root(), &s, &online()).expect("view");
            if expect_foreign {
                assert_eq!(view.foreign_source_refs.len(), 1, "{view:?}");
                assert_eq!(view.foreign_source_refs[0].foreign, Some(Foreign::NoIdEver));
                assert_eq!(view.unreflected_pub, vec![pub_sha]);
                assert!(!view.related, "nothing of ours: first contact");
            } else {
                assert_eq!(view.broken_source_refs.len(), 1, "{view:?}");
                assert_eq!(view.broken_source_refs[0].unplaced, Some(Unplaced::NoId));
                assert!(view.unreflected_pub.is_empty());
                assert!(view.unplaced_only);
            }
        }
    }

    /// Everything at or below a standalone commit this monorepo imported is settled, whoever
    /// wrote it. Above our own verified export, an id-less claim naming nothing here stays
    /// unplaced for a monorepo without an id — no earlier claim, imported or fetched, turns it
    /// into "another publisher's".
    #[test]
    fn below_an_import_claims_are_settled_and_nothing_latches() {
        let f = Fixture::new("settled-no-latch");
        let tree = f.sh("git rev-parse HEAD:core");
        let published = f.push_pub(
            &tree,
            None,
            &format!("their release\n\nMonosplice-Source: {}\n", "1".repeat(40)),
        );
        let attach = f.commit(&format!("Adopt core\n\nMonosplice-Origin: {published}\n"));
        f.sh("printf 'patch\n' > core/patch.txt");
        let patch = f.commit("fix: our patch");
        let patch_tree = f.sh("git rev-parse HEAD:core");
        let exported = f.push_pub(
            &patch_tree,
            Some(&published),
            &format!("fix: our patch\n\nMonosplice-Source: {patch}\n"),
        );
        let later = f.push_pub(
            &patch_tree,
            Some(&exported),
            &format!(
                "their next release\n\nMonosplice-Source: {}\n",
                "2".repeat(40)
            ),
        );

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.superseded_source_refs.len(), 1, "{view:?}");
        assert_eq!(view.superseded_source_refs[0].pub_sha, published);
        assert_eq!(view.broken_source_refs.len(), 1);
        assert_eq!(view.broken_source_refs[0].pub_sha, later);
        assert_eq!(view.broken_source_refs[0].unplaced, Some(Unplaced::NoId));
        assert!(view.foreign_source_refs.is_empty());
        assert_eq!(view.last_exported_mono.as_deref(), Some(patch.as_str()));
        assert_eq!(view.export_base.as_deref(), Some(patch.as_str()));
        assert_ne!(view.export_base.as_deref(), Some(attach.as_str()));
        assert!(view.unreflected_pub.is_empty());
        assert!(!view.unplaced_only);
    }

    /// A Source line copied onto standalone work that sits on unimported work and does not
    /// reproduce the commit it names is not our export: it and everything below it are to pull.
    /// Only a *verified* export settles what is below it.
    #[test]
    fn a_copied_line_is_not_ours_and_settles_nothing() {
        let f = Fixture::new("copied-line");
        let head = f.sh("git rev-parse HEAD");
        let tree = f.sh("git rev-parse HEAD:core");
        let e0 = f.push_pub(
            &tree,
            None,
            &format!("first commit\n\nMonosplice-Source: {head}\n"),
        );
        let blob = f.sh("printf 'contrib\n' | git hash-object -w --stdin");
        let t1 = f.sh(&format!(
            "(git ls-tree {tree}; printf '100644 blob {blob}\\tc1.txt\\n') | git mktree"
        ));
        let c1 = f.push_pub(&t1, Some(&e0), "contrib 1\n");
        let c2 = f.push_pub(
            &t1,
            Some(&c1),
            &format!("contrib 2\n\nMonosplice-Source: {head}\n"),
        );
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(view.broken_source_refs.is_empty(), "{view:?}");
        assert_eq!(view.copied_source_refs.len(), 1);
        assert_eq!(view.copied_source_refs[0].pub_sha, c2);
        assert_eq!(view.unreflected_pub, vec![c1, c2]);
        assert_eq!(view.export_base.as_deref(), Some(head.as_str()));
    }

    /// A commit monosplice fetched from another standalone repository resolves here, but it is
    /// that repository's commit: never an anchor, never a rewritten one.
    #[test]
    fn a_claim_naming_a_fetched_commit_of_another_repository_is_foreign() {
        let f = Fixture::new("fetched-foreign");
        let tree = f.sh("git rev-parse HEAD:core");
        // Another repository's commit, present only under a monosplice tracking ref.
        let blob = f.sh("printf 'middle\n' | git hash-object -w --stdin");
        let middle_tree = f.sh(&format!(
            "printf '100644 blob {blob}\\tmain.txt\\n' | git mktree"
        ));
        let middle = f.sh(&format!(
            "printf 'middle: work\n' | git commit-tree {middle_tree}"
        ));
        f.sh(&format!(
            "git update-ref refs/monosplice/middle/remote {middle}"
        ));
        let pub_sha = f.push_pub(
            &tree,
            None,
            &format!("middle: work\n\nMonosplice-Source: {middle}\n"),
        );

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.foreign_source_refs.len(), 1);
        assert_eq!(view.foreign_source_refs[0].pub_sha, pub_sha);
        assert_eq!(view.last_exported_mono, None, "not a rewritten anchor");
        assert!(view.broken_source_refs.is_empty());
        assert!(!view.related);
    }

    /// Only the last sync trailer of a commit is its own; the ones before it were forwarded
    /// from an earlier hop and name some other repository's commits.
    #[test]
    fn forwarded_trailers_are_ignored_and_counted() {
        let f = Fixture::new("forwarded");
        let mono = f.sh("git rev-parse HEAD");
        let tree = f.sh("git rev-parse HEAD:core");
        let outer = "0".repeat(40);
        let pub_sha = f.push_pub(
            &tree,
            None,
            &format!("outer: patch\n\nMonosplice-Source: {outer}\nMonosplice-Source: {mono}\n"),
        );
        let leaf = "1".repeat(40);
        f.commit(&format!(
            "leaf: add b\n\nMonosplice-Origin: {leaf}\nMonosplice-Origin: {pub_sha}\n"
        ));

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(view.broken_source_refs.is_empty(), "{view:?}");
        assert!(view.foreign_source_refs.is_empty());
        assert_eq!(view.forwarded_source_trailers, 1);
        assert_eq!(view.forwarded_origin_trailers, 1);
        assert_eq!(view.last_exported_mono.as_deref(), Some(mono.as_str()));
        assert!(!view.imported_pub_shas.contains(&leaf));
        assert!(view.imported_pub_shas.contains(&pub_sha));
    }

    /// Validation stops at the newest trailer that resolves. Below that point a dead sha is a
    /// fossil of somebody's rebase — no clone will ever have it, and it cannot change what is
    /// published. Above it, the mapping is unreadable and the refusal stands.
    #[test]
    fn a_dead_source_ref_is_superseded_below_the_live_anchor_and_broken_above_it() {
        let f = Fixture::new("superseded-source");
        let mono = f.sh("git rev-parse HEAD");
        let tree = f.sh("git rev-parse HEAD:core");
        let rebased_away = "0".repeat(40);
        let elsewhere = "1".repeat(40);

        let old = f.push_pub(
            &tree,
            None,
            &format!("export from before a rebase\n\nMonosplice-Source: {rebased_away}\n"),
        );
        let live = f.push_pub(
            &tree,
            Some(&old),
            &format!("export that healed it\n\nMonosplice-Source: {mono}\n"),
        );

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(
            view.broken_source_refs.is_empty(),
            "{:?}",
            view.broken_source_refs
        );
        assert_eq!(view.superseded_source_refs.len(), 1);
        assert_eq!(view.superseded_source_refs[0].pub_sha, old);
        assert_eq!(view.superseded_source_refs[0].mono_sha, rebased_away);
        assert_eq!(view.last_exported_mono.as_deref(), Some(mono.as_str()));
        assert_eq!(view.export_base.as_deref(), Some(mono.as_str()));

        // Now a public commit above the live anchor names a commit nobody here has: that
        // stretch of public history cannot be checked, and the refusal comes back.
        f.push_pub(
            &tree,
            Some(&live),
            &format!("published from somewhere else\n\nMonosplice-Source: {elsewhere}\n"),
        );
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.broken_source_refs.len(), 1);
        assert_eq!(view.broken_source_refs[0].mono_sha, elsewhere);
        assert_eq!(
            view.superseded_source_refs.len(),
            1,
            "still just the fossil"
        );
        assert_eq!(view.last_exported_mono.as_deref(), Some(mono.as_str()));
    }

    #[test]
    fn unreflected_pub_is_ancestry_based_not_per_commit() {
        let f = Fixture::new("unreflected");
        let blob1 = f.sh("printf 'one\n' | git hash-object -w --stdin");
        let t1 = f.sh(&format!(
            "printf '100644 blob {blob1}\\ta.txt\\n' | git mktree"
        ));
        let p1 = f.push_pub(&t1, None, "pub one\n");
        let blob2 = f.sh("printf 'two\n' | git hash-object -w --stdin");
        let t2 = f.sh(&format!(
            "printf '100644 blob {blob1}\\ta.txt\\n100644 blob {blob2}\\tb.txt\\n' | git mktree"
        ));
        let p2 = f.push_pub(&t2, Some(&p1), "pub two\n");
        let blob3 = f.sh("printf 'three\n' | git hash-object -w --stdin");
        let t3 = f.sh(&format!(
            "printf '100644 blob {blob1}\\ta.txt\\n100644 blob {blob2}\\tb.txt\\n100644 blob {blob3}\\tc.txt\\n' | git mktree"
        ));
        let p3 = f.push_pub(&t3, Some(&p2), "pub three\n");

        // Nothing imported yet: all three are pending, oldest first.
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(
            view.unreflected_pub,
            vec![p1.clone(), p2.clone(), p3.clone()]
        );

        // Import only the middle one. Its ancestor p1 is reflected by construction.
        f.commit(&format!("import\n\nMonosplice-Origin: {p2}\n"));
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.unreflected_pub, vec![p3.clone()]);
    }

    #[test]
    fn a_forged_origin_value_cannot_abort_the_pending_walk() {
        let f = Fixture::new("forged-origin");
        let blob = f.sh("printf 'one\n' | git hash-object -w --stdin");
        let t1 = f.sh(&format!(
            "printf '100644 blob {blob}\\ta.txt\\n' | git mktree"
        ));
        let p1 = f.push_pub(&t1, None, "pub one\n");
        f.commit(&format!(
            "import\n\nMonosplice-Origin: {}\n",
            "0".repeat(40)
        ));

        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert_eq!(view.unreflected_pub, vec![p1]);
    }

    #[test]
    fn our_own_exports_never_read_as_pending_imports() {
        let f = Fixture::new("own-exports");
        let mono = f.sh("git rev-parse HEAD");
        let tree = f.sh("git rev-parse HEAD:core");
        f.push_pub(
            &tree,
            None,
            &format!("first commit\n\nMonosplice-Source: {mono}\n"),
        );
        let view = load_sync_view(f.root(), &f.subrepo(), &online()).expect("view");
        assert!(view.unreflected_pub.is_empty());
    }

    // --- fork state ---

    #[test]
    fn fork_state_is_none_until_the_push_branch_exists_and_then_mirrors_it() {
        let f = Fixture::new("fork-state");
        let mut s = f.subrepo();
        s.upstream = Some("upstream-does-not-matter-here".to_string());
        s.push_branch = "patches".to_string();

        assert_eq!(load_fork_state(f.root(), &s, &online()).unwrap().head, None);

        let head = f.sh("git rev-parse HEAD");
        f.sh(&format!(
            "git push -q {} {head}:refs/heads/patches",
            f.remote_url()
        ));
        let state = load_fork_state(f.root(), &s, &online()).expect("fork state");
        assert_eq!(state.head.as_deref(), Some(head.as_str()));
        // The fetch really mirrored it into the fork tracking ref, so offline agrees.
        let offline = load_fork_state(f.root(), &s, &SyncViewOptions { offline: true }).unwrap();
        assert_eq!(offline.head.as_deref(), Some(head.as_str()));
    }

    #[test]
    fn an_unreachable_fork_is_a_note_not_a_crash() {
        let f = Fixture::new("fork-unreachable");
        let mut s = f.subrepo();
        s.remote = f.dir.join("nope.git").display().to_string();
        let (state, error) = try_load_fork_state(f.root(), &s, &online());
        assert!(state.is_none());
        assert!(error.is_some(), "expected a GitError");
        assert!(load_fork_state(f.root(), &s, &online()).is_err());
    }
}
