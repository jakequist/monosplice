# E2E scenario backlog

Living TDD backlog. Each scenario becomes a black-box test in `test/e2e/` that drives the
built CLI against throwaway git repos (local bare repos as "public" remotes). Check items
off as their tests land. IDs are stable — reference them in commits and test names.

Since the Rust rewrite these S-numbers live in `tests/e2e_*.rs` (Rust, black-box over
`CARGO_BIN_EXE_monosplice`), not `test/e2e/`; config examples in older scenarios below —
`monosplice.config.ts`, `pushBranch`, function hooks — read as their `monosplice.toml`
equivalents (`push-branch`, shell-command hooks; see docs/reference.md).

Conventions: **mono** = the private monorepo, **pub** = the public bare remote for a
subrepo, `core/` = the configured subrepo path.

## Init & first publish

> Reworked when `seed` was retired: outbound first contact now lives in `push`
> (confirmation-gated) and in `attach`; inbound first contact is `attach` (see S90s, S130s).

- [x] S01 `init` scaffolds `monosplice.config.ts`; running it again is a safe no-op.
- [x] S02 First `push --yes` (baseline): mono with mixed history → pub gets exactly one baseline commit whose tree equals `core/` subtree, carrying `Monosplice-Source`.
- [x] S03 First `push --yes --export-history`: every mono commit touching `core/` replayed in order with messages/authors preserved and `Monosplice-Source` trailers.
- [x] S04 First push honors `exclude` patterns — excluded files absent from pub tree even though present in mono history.
- [x] S05 Push against a pub that has history but no relation to mono → refuses, points at `monosplice attach`, exit ≠ 0.
- [x] S06 First push when `core/` has no committed files yet → clear error, nothing pushed.

## Push (export)

- [x] S10 One new mono commit touching `core/` → `push` creates one pub commit: same message, same author, tree = subtree, trailer appended.
- [x] S11 Commits touching only private dirs (`website/`) are not exported.
- [x] S12 A commit spanning `core/` + private dirs exports with only the `core/` subtree (private paths never in pub objects).
- [x] S13 Multiple pending commits export in order; pub log order matches mono order.
- [x] S14 `push` twice → second run is a no-op ("up to date"), zero new pub commits, exit 0.
- [x] S15 Modifying an excluded file exports nothing; if the commit *only* touched excluded files, no empty pub commit is created (commit skipped).
- [x] S16 `rewriteMessage` hook in config is applied to exported commit messages.
- [x] S17 Pure imports are tree-no-ops on push (dropped by the tree-equality check, not by trailer) — no ping-pong duplicates in pub.
- [x] S18 Binary files, file deletions, and renames replay correctly (tree equality after each commit).
- [x] S19 Executable bit and symlinks are preserved in exported trees.
- [x] S20 Pub has an unimported external commit → `push` refuses, tells user to `monosplice pull` first; pub untouched.
- [x] S21 Secret-scan hook rejects a commit → push aborts *before* any ref update on pub; error names the offending commit/file.
- [x] S22 `transform` hook mutates the exported tree (e.g., swaps README) without affecting mono.

## Pull (import)

- [x] S30 External commit in pub → `pull` creates a mono commit placing the tree under `core/`, original author preserved, `Monosplice-Origin` trailer added.
- [x] S31 Multiple upstream commits import in order.
- [x] S32 `pull` twice → second run is a no-op.
- [x] S33 Pub commits carrying `Monosplice-Source` (our own exports) are skipped on pull.
- [x] S34 Uncommitted local changes under `core/` (or anything staged anywhere) → `pull` refuses before touching anything.
- [x] S35 Conflicting edits (same file changed in mono and pub) → conflict markers in mono working tree, clear instructions, and after `git add` + `monosplice pull --continue` the import lands and the resolution round-trips back to pub on the next push.
- [x] S36 External commit adds a file matching an `exclude` pattern → defined behavior (import + warn that the next push deletes it from pub), covered by test so the decision is locked in.

## Sync & convergence

- [x] S40 `sync` = pull then push; from divergence with non-conflicting changes, one command converges both repos.
- [x] S41 Round-trip fidelity: after any sync, pub HEAD tree is byte-identical to mono `core/` subtree (minus excludes).
- [x] S42 Stability: push → pull → push → pull produces zero new commits after the first cycle (fixed point reached).
- [x] S43 Interleaved history (mono and pub alternate commits over several rounds) converges with every commit present exactly once on each side. Known exception, locked in by the test: in a round where *both* sides moved, the import sits on top of the local commit, so its resolution is re-exported and that subject appears twice in pub (same rule that preserves conflict resolutions).

## Status, state & doctor

- [x] S50 `status` reports per-subrepo ahead/behind counts (N unexported, M unimported) and "in sync" when clean.
- [x] S51 No state file exists by design — after arbitrary push/pull cycles, deleting nothing is possible; instead verify all cursors derive from trailers: `doctor` reports the derived sync points and they match reality.
- [x] S52 Broken mapping (pub trailer referencing a mono sha that doesn't exist locally) → `doctor` detects and reports it clearly.
- [x] S53 Fresh clone of mono in a new directory ("second machine") → `status`/`push`/`pull` work immediately with no state to restore.
- [x] S54 Mono main was rebased/force-pushed (cursor no longer an ancestor of HEAD) → loud error naming the problem; nothing exported.
- [x] S55 Mono main was rebased over unrelated commits — the anchor sha is gone but the subrepo tree is byte-identical → `push` re-derives the anchor from that tree (`recovered anchor: <old> → <new>`), exports only the new commit, and is ordinary again afterwards; `doctor` reports the recovery it would do as a note; a rewrite that changed the subrepo tree still refuses, and unimported standalone commits still refuse.
- [x] S56 A clone made *after* someone else's rebase is missing the sha an older public commit still names, while the public tip's anchor resolves → validation stops at the newest resolvable anchor: `push` works, `doctor` exits 0 and reports the dead trailer as informational ("superseded by live anchor at <sha>"). A dead anchor with nothing resolvable above it (shallow clone, wrong remote) still refuses.
- [x] S57 A subrepo re-baselined onto a brand-new remote: monorepo history still carries `Monosplice-Origin` trailers from the previous remote → below the live export anchor they are informational ("N historical import trailer(s) unresolvable — superseded by live anchor at <sha>"), `doctor` exits 0 and push/dry-run are clean. Above the live anchor, or with the config pointed at an unrelated remote (nothing anchored), the orphan is still a problem and `doctor` still exits 1.

## Multi-subrepo

- [x] S60 Two subrepos with separate pub remotes → `push` exports each to its own remote only.
- [x] S61 `push core` (named) touches only that subrepo's remote and cursor.
- [x] S62 One mono commit touching both subrepos exports to both pubs, each with only its own subtree.

## Tags

- [x] S70 `monosplice tag core v1.0.0` resolves the current mapping and tags the corresponding pub commit; tag visible on pub.
- [x] S71 Tagging when unexported commits exist → warn/refuse (tag would not match mono HEAD).

## Robustness & UX

- [x] S80 Running any command outside a monosplice-configured repo → helpful error, exit ≠ 0.
- [x] S81 Invalid config (bad path, missing remote, malformed exclude) → validation errors name the field and file.
- [x] S82 Subrepo `remote` unreachable → clean error surfaced with the git detail, no partial state written.
- [x] S83 A `.gitignore` inside `core/` is exported like any other file; mono root ignores do not leak into pub.
- [x] S84 Unicode filenames and messages survive round-trip export/import.
- [x] S85 `--json` output for `status` is stable and machine-parseable (locks the contract for CI use).

## First contact & adoption (auto-detection matrix)

> The user never needs to know monosplice internals: outbound t=0 is a
> confirmation-gated `push`, inbound t=0 is `attach` (shallow by default).
> "Baseline" = the sync point; nothing in mono history is ever squashed.
>
> These scenarios were written against the retired `adopt` command; the behaviour is
> unchanged and now reached through `attach` (S130+).

- [x] S90 `push` with an unpublished subrepo, non-interactive, no `--yes` → refuses with a one-line explanation and the exact command to run; remote stays empty; other subrepos in the same run are still pushed.
- [x] S91 `push --yes` publishes the baseline and reports it distinctly from normal exports; an immediately following `push` is "up to date"; later commits export per-commit.
- [x] S92 `push --yes --export-history` replays all history; scan hooks run per replayed commit and a throwing hook aborts with nothing pushed (the dead-secret case).
- [x] S93 `adopt <name>` with pub history and NO mono dir (shallow default): exactly ONE mono commit placing pub HEAD's tree at the path, `Monosplice-Origin: <pubHead>`; then `pull`, `push`, `status` all report in sync (ancestry-based reflection — pub's 50-commit history must NOT show as "50 to pull"). *(superseded by S130 via `attach`)*
- [x] S94 `adopt <name> --import-history`: full per-commit import with authors/messages preserved (the old pull-adopt behavior), then in sync. *(superseded by S131 via `attach`)*
- [x] S95 `adopt` when BOTH sides have content and trees match exactly → baseline recorded (empty mono commit with Origin trailer); push/pull in sync; a NEW mono commit then exports parented on the EXISTING pub head (shared history going forward). *(superseded by S132 via `attach`)*
- [x] S96 `adopt` when both sides have content and trees differ → refuses listing the differing paths, nothing written anywhere; `adopt --theirs` replaces the mono dir with pub content in one commit (Origin trailer) and lands in sync; the pre-adopt mono content stays in mono history but never exports. *(superseded by S132 via `attach`)*
- [x] S97 `pull` against an unrelated pub (no trailers, mono dir exists) → refuses and points at `attach`; nothing imported, working tree untouched.
- [x] S98 REGRESSION GUARD: after any adopt (S93/S95/S96), `push` must never re-export pre-adoption mono history — the export scan base must anchor on the adopt commit's Origin trailer, not just pub Source trailers. Assert pub log gains nothing but genuinely new commits. *(now driven through `attach`)*
- [x] S99 Matrix dead end: configured subrepo with empty mono dir AND empty remote → every command gives the same clear "nothing exists yet" error, exit ≠ 0.
- [x] S99a `adopt` preconditions: dirty files under the path, or staged changes anywhere → refuses before fetching/writing (same rules as pull). *(superseded by S135 via `attach`)*
- [x] S99b `adopt` on an already-related subrepo (trailers exist) → "already adopted/published", no-op, exit ≠ 0 with explanation. *(superseded by S134 via `attach`)*

## Vendor

> Every scenario in this section is now reached through `attach <folder> <url>` — the
> `vendor` command is gone. The behaviour under test is unchanged; only the invocation moved,
> so the IDs stay put. See S130+ for what `attach` added.

- [x] S100 `vendor <url>`: creates `vendor/<name>/` from the remote's HEAD tree, appends a valid entry to monosplice.config.ts, and commits BOTH in a single commit carrying `Monosplice-Origin: <pubHead>`; `status` in sync; `pull`/`push` up to date. (`--path`/`--name`/`--branch` covered by the same describe.)
- [x] S101 Upstream advances → `pull` imports the new commits into `vendor/<name>/` per-commit.
- [x] S102 Local patch to a vendored file + upstream change to a different file → `pull` three-way merges cleanly; both changes present; nothing left to pull. Locked in by the test: `status` says **2** to push, not 1 — both sides moved, so the import sits on top of the local patch and its tree differs from the public tip, which is the same rule as S43. Pushing converges the trees.
- [x] S103 Local patch + upstream edit to the SAME line → conflict markers under `vendor/<name>/`, `pull --continue` completes, trees converge after push.
- [x] S104 `vendor` with a name or path already in config → refuses; config byte-identical, no commit created.
- [x] S105 `vendor` with a dirty working tree, staged changes, an existing directory at the target path, or a path nesting inside a configured subrepo → refuses before fetching or writing anything.
- [x] S106 `vendor` with an unreachable URL or missing branch → clean error; config untouched, no commit, no directory.
- [x] S107 Config-append safety: a monosplice.config.ts whose shape the inserter can't parse → vendor makes NO changes and prints the exact config snippet to paste manually (on stdout, exit ≠ 0).

## Triangular remotes (fork PR workflow)

> Optional per-subrepo `upstream` (fetch/pull source, e.g. lodash/lodash);
> `remote` becomes the push destination (your fork); optional `pushBranch`
> (default: `branch`). Without `upstream`, behavior is byte-for-byte today's.

- [x] S110 With `upstream` set: `pull` imports from upstream even when the fork remote is empty or stale; the fork is never fetched for import decisions.
- [x] S111 `push` exports local patches to `remote`'s `pushBranch`, parented on the UPSTREAM head (PR-ready: fork branch is upstream + patches, linear); upstream repo is never written to.
- [x] S112 Upstream advances while local patches exist: `sync` imports upstream then re-exports patches on top of the new upstream head, updating the fork branch (force-with-lease — the branch is ours); resulting fork branch = upstream head + patches, nothing lost.
- [x] S113 No `upstream` configured → push/pull/status behavior identical to before (explicit regression flow, non-force push preserved).
- [x] S114 Unreachable upstream vs unreachable fork remote → two distinct, correctly-attributed error messages; status attributes each side.
- [x] S115 `vendor <upstream-url> --fork <fork-url>` writes both `upstream` and `remote` (+ default pushBranch) in the config entry; pull comes from upstream, push goes to fork. *(superseded by S138: `attach <folder> <upstream-url> --fork <fork-url>`)*
- [x] S116 PR merged upstream as a fast-forward/merge (exported commits with their `Monosplice-Source` trailers land in upstream) → `pull` is a no-op, fixed point holds.
- [x] S117 PR squash-merged upstream (same tree, new commit, trailers lost) → `pull` records it (possibly as an empty import), `push` stays up to date; no ping-pong, trees converged.
- [x] S118 `status`/`doctor` with `upstream`: ahead/behind measured against upstream; doctor fetches and reports both sides without false alarms.

## Attach (one-command first contact)

> `monosplice attach <folder> <repo-url>` — write the config entry connecting `<folder>` to
> `<repo-url>`, then make the right first-contact move automatically. Same detection matrix,
> same safety rails, zero hand-editing.

- [x] S120 `attach <folder> <url>` with no committed content at the folder and a remote that has history → ONE commit carrying both the config entry and the remote HEAD's tree at `<folder>`, with `Monosplice-Origin: <pubHead>`; `status` in sync; `pull`/`push` up to date. Works for nested paths (`packages/lib`). `--name`/`--branch` override the defaults (name defaults to the last path segment).
- [x] S121 `attach <folder> <url>` with committed content at the folder and an EMPTY remote → the config entry is committed on its own, then first-push semantics: `--yes` publishes the baseline (`--export-history` replays every commit that touched the folder); without `--yes` (non-interactive) the config commit still lands, the error names `monosplice push <name> --yes`, exit ≠ 0; running that push then converges.
- [x] S122 `attach` with committed content at the folder and a remote whose tree MATCHES → one commit: config entry + adopt baseline (`Monosplice-Origin`), in sync immediately; a later mono commit exports parented on the existing pub head.
- [x] S123 `attach` with committed content and a remote whose tree DIFFERS → refuses listing the differing paths; config byte-identical, no commit. `attach --theirs` takes the remote tree in the same single commit (config + tree + Origin trailer).
- [x] S124 `attach` refusals leave the config byte-identical and make no commit: name or path already configured, path nesting inside a configured subrepo, dirty working tree or staged changes, pull sequencer in progress.
- [x] S125 `attach` with an unreachable URL → clean error, nothing written. Folder empty AND remote empty → the shared "nothing exists yet" error, config untouched.
- [x] S126 `attach` on a config whose shape the inserter can't parse → NO changes, prints the exact snippet to paste (stdout, exit ≠ 0), and names the follow-up command per case (`attach <folder>` for existing remote history, `push --yes` for an empty remote).

## Attach consolidation

> `attach` absorbed `adopt` and `vendor`; both commands are gone. The URL argument is
> optional: with a folder that already matches a configured entry, `attach <folder>` makes
> first contact for it and writes nothing to the config. `--import-history` (from `adopt`) and
> `--fork` (from `vendor`) came along with them.

- [x] S130 `attach <folder>` with NO url on a configured entry, pub history + empty/absent folder → exactly ONE mono commit (`Adopt <name> from …`, `Monosplice-Origin: <pubHead>`), the config file byte-identical, then `status`/`pull`/`push` all in sync (a 20-commit pub must not read as "20 to pull"). Resolves the entry by path *or* by name.
- [x] S131 `--import-history` in both entry modes: `attach <folder>` on a configured entry replays every public commit with authors/messages preserved; `attach <folder> <url> --import-history` on a NEW entry commits the config entry on its own first and then replays. Refuses (nothing written) when the folder already has committed files, or when the remote has no branch.
- [x] S132 `attach <folder>` on a configured entry with content: trees match → empty baseline commit with the Origin trailer, in sync, later commits export on the existing pub head; trees differ → refuses listing the differing paths and writes nothing, `--theirs` takes the public tree in one commit.
- [x] S133 `attach <folder>` on a configured entry whose remote is EMPTY → gated first publish: `--yes` publishes the baseline (`--export-history` replays), non-interactive without `--yes` refuses naming `monosplice push <name> --yes` and publishes nothing. Both sides empty → the shared "nothing exists yet" error.
- [x] S134 `attach <folder>` on an already-related subrepo (trailers exist) → "already connected", no-op, exit ≠ 0, naming pull/push/sync.
- [x] S135 `attach <folder>` preconditions on a configured entry: dirty files under the path, or staged changes anywhere → refuses before fetching or writing (same rules as `pull`), no tracking ref created.
- [x] S136 `attach <folder> <url>` where the folder is already configured: a url equal to the configured remote (or to `upstream` when set) proceeds exactly as the url-less form; a different url refuses, naming the configured remote and pointing at the config file, with nothing changed.
- [x] S137 `attach <folder>` with no url and no matching entry → error saying attach needs a url to create an entry, listing the configured subrepos; config byte-identical, no commit.
- [x] S138 `attach <folder> <upstream-url> --fork <fork-url>` writes `upstream` + `remote` (+ default pushBranch), takes the tree and the anchor from **upstream**, pulls from upstream and pushes to the fork; upstream is never written to. `--fork` equal to the url refuses; `--fork` on an already-configured entry refuses and names the config edit instead.
- [x] S139 Write-access probe (advisory, never blocking): attaching to a writable remote with history prints no advisory and exits 0; attaching to a remote that can be fetched but refuses `git push` still succeeds (exit 0, commit made) and prints an advisory on stderr naming the triangular re-run with `--fork`. Skipped entirely with `--fork` and on an empty remote.
- [x] S140 `monosplice adopt` and `monosplice vendor` no longer exist → oclif's unknown-command error, exit ≠ 0; and no user-facing string in the built CLI names either command.

## CLI ergonomics

> An external review of the CLI surface. The theme is the same throughout: every dead end
> must name the command that gets you out of it, and every command must be usable from a
> script without parsing prose.

- [x] S150 `pull --abort` after a conflict restores the monorepo to exactly its pre-pull state — the imports this run committed are rewound, `core/` and the index are back to the pre-pull tree, and nothing outside `core/` (unstaged edits, untracked files) is touched; the sequencer is gone and a following `pull` starts clean. When monorepo history moved after the conflict, abort drops only the conflicted step, keeps (and names) the commits it cannot prove are its own. `--abort` with no pull in progress, and `--abort --continue` together, both refuse with exit ≠ 0 and nothing changed.
- [x] S151 Flag rename: `--import-history` (replay the standalone repo's commits inwards) and `--export-history` (replay monorepo commits outwards on a first publish) are the only spellings — the old `--history` and `--full-history` are unknown flags on `attach` and `push`, exit ≠ 0, and each flag's `--help` text names the other.
- [x] S152 A config with zero subrepos → `status`, `push`, `pull` and `sync` each print `no subrepos configured — run \`monosplice attach <folder> <git-url>\` to connect one` and exit 0; `status --json` still emits valid JSON (`{"subrepos":[]}`) and nothing else.
- [x] S153 `status --check` exits 1 unless every subrepo is fully in sync (nothing to push, nothing to pull, no unreachable remote) and 0 when they are; the human output is byte-identical to `status` without the flag, and `--check --json` keeps stdout pure JSON.
- [x] S154 `doctor --json` emits one stable machine-readable object on stdout and nothing else (no human report), with the same key set every run; the exit code is unchanged (0 clean, 1 with problems).
- [x] S155 Multi-subrepo failure policy is uniform: `pull` and `sync` collect per-subrepo failures and keep going like `push`, reporting them all at the end with exit ≠ 0 — except an import conflict, which writes the sequencer and therefore stops the run immediately, naming `--continue`/`--abort`.
- [x] S156 Wording and streams: no command description or user-facing message calls the other repo "public" (it is the "standalone" repo); `status`'s `!` diagnostics go to stderr so stdout stays pipeable, and `--json` output stays pure JSON on stdout.

## Pre-release polish

> The last batch before v1: previewing a run, undoing an attach, working without the
> network, finishing a `sync` that stopped on a conflict, and a config file you can write
> without TypeScript.

- [x] S160 `push --dry-run` / `pull --dry-run` print exactly what would move — one `<short-sha> <subject>` line per pending commit in the direction's order, under a summary line marked `(dry run — nothing written)` — and write nothing anywhere: no remote ref, no monorepo commit, no working-tree or index change, and the tracking ref is left where the read path put it. Nothing pending prints the up-to-date line. Both exit 0, and a following real run still moves the same commits. Scan/transform hooks do NOT run on a dry run (the count is what would be *attempted*); `push --help` says hooks still gate the real push.
- [x] S161 `detach <subrepo>` removes the entry from the config and commits that edit alone (`Detach <name>: stop tracking <url>`): the subrepo's files and every commit in monorepo history survive untouched, past trailers become inert, and the output says so and names `monosplice attach <path> <url>` with the URL it was tracking. Afterwards `status`/`push`/`pull` no longer mention the subrepo. Refuses — config byte-identical, no commit — on an unknown subrepo, a pull sequencer for it, staged changes or a dirty working tree. Never contacts the network. A config the remover cannot locate the entry in textually is restored byte-for-byte and the user is told exactly what to delete (exit ≠ 0).
- [x] S162 `status --offline` fetches nothing: it reports from the existing remote-tracking refs, says `offline: using last-fetched state` once per run on stderr, and reports a subrepo with no tracking ref yet as `no fetch yet — run without --offline first` instead of guessing. A remote that moved after the last fetch is invisible to `--offline` and visible again without it. Combines with `--json` (top-level `offline: true`, row key set unchanged) and with `--check`.
- [x] S163 `monosplice autocomplete` exists (oclif's autocomplete plugin) and `monosplice autocomplete --help` exits 0; the README's Install section names it.
- [x] S164 `sync --continue` finishes an interrupted pull exactly like `pull --continue` and then runs the push phase for every subrepo, converging the ones whose pull had already succeeded when the conflict stopped the run. `sync`'s conflict error names `monosplice sync --continue` (and `monosplice pull --abort` still aborts); `sync --continue` with no pull in progress refuses with pull's wording, exit ≠ 0.
- [x] S165 `monosplice init` writes `monosplice.config.js` (ESM `export default` + a JSDoc `@type` annotation) and the loader treats `.js` as first-class alongside `.ts`/`.mts`/`.mjs`/`.cjs`. Two config files in one directory (any two of `monosplice.config.{js,ts,mjs,mts,cjs}`) make **every** command error, naming both files and telling the user to delete one.
- [x] S166 A leading `./` on a user-supplied subrepo path is tolerated everywhere it can be typed: `monosplice attach ./core <url>` works exactly like `attach core <url>` (the config entry records `core`). Bare `.` and `..` segments stay rejected.
