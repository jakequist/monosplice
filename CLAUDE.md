# monosplice

CLI (Rust, single static binary) for managing monorepos that publish subdirectories as
standalone open-source repos, with bidirectional sync. Think "git subtree with Copybara's
brain and submodule-free ergonomics."

## Architecture (the invariants — do not violate these)

- **Model B / two-histories design.** The monorepo and each public subrepo have *independent*
  histories. Monosplice replays commits across the boundary and records the correspondence.
  We never try to make the public repo a deterministic filter of monorepo history.
- **Trailers are the source of truth for the commit mapping.**
  - Exported public commits carry `Monosplice-Source: <monorepo-sha>`.
  - Imported monorepo commits carry `Monosplice-Origin: <public-sha>` — the marker that a pub
    commit is reflected in mono (so `pull` skips it and `push` stops refusing).
  - Import skips pub commits with `Monosplice-Source` (our own exports).
  - Export does NOT skip by trailer: a pure import's tree already equals the pub tip, so the
    tree-equality no-op check drops it; a *conflicted* import (merge of mono + pub edits)
    differs from the pub tip and MUST export, or the resolution would be lost. This is what
    keeps `pub tree == filtered(mono HEAD)` structurally true after every push.
  - **Origin trailers are export anchors too.** The export scan base is the newest commit on
    the `HEAD` walk that pub already contains: a `Monosplice-Source` key, *or* a
    `Monosplice-Origin` commit naming an ancestor of pub head whose `filteredSubtree` equals
    that pub commit's tree. The tree check is load-bearing both ways — without it a push
    after `adopt` replays the whole pre-adoption monorepo history onto the adopted repo, and
    with a naive version a conflicted import would silently become the base and lose its
    resolution.
  - **Reflection is ancestry-based.** Unimported pub commits are
    `rev-list --first-parent <pubHead> --not <each imported sha>` (fed via `--stdin`), minus
    our own exports. Every ancestor of a reflected commit is reflected; a one-commit `adopt`
    of a 200-commit repo must never read as "200 to pull".
  - **Imports follow pub's first-parent line.** A merge is one import step (its diff against
    its first parent is upstream's own resolution); side-branch commits are never replayed on
    their own — doing so re-creates conflicts upstream already resolved.
- **Triangular mode: upstream decides, the fork is disposable.** When a subrepo configures
  `upstream`, every sync decision (fetch, anchors, unreflected, ahead/behind) is made against
  upstream and *only* upstream — the fork is never consulted for imports. `remote` is the push
  destination: monosplice rebuilds its `pushBranch` as upstream head + replayed patches on every
  push and writes it with `--force-with-lease`, because that branch is a derived artifact
  monosplice owns. Upstream is never written to — no branch, no tag.
- **First contact is detected, not configured.** Outbound (`pubHead` null) is a
  confirmation-gated first `push` — TTY prompt, `--yes` otherwise, plus `--export-history`.
  Inbound (pub has unrelated history) is `adopt`. Unrelated + either direction → refuse and
  name `monosplice adopt`. Both empty → one shared "nothing exists yet" error. There is no
  `seed` command.
- **There is no authoritative state file.** Sync cursors are derived from trailers on every
  run (scan pub for `Monosplice-Source`, mono for `Monosplice-Origin`). Correctness must never
  depend on cached state; any cache added later is a pure optimization that `doctor` can
  verify. Fresh clones work with zero ceremony because of this.
- **Export never touches the working tree.** All export work uses git plumbing
  (`rev-list`, `commit-tree`, `mktree`, `cat-file`) against the object database. Import is the
  only operation allowed to modify the working tree (it's a merge the user resolves) — and
  `adopt` is an import-side op, so it may too.
- **Talk to git by shelling out** (`std::process::Command` + system `git`). No git library,
  ever — not libgit2, not gitoxide.
- **Fail loud on surprises.** Cursor not an ancestor of HEAD (rebase/force-push), diverged
  public remote, dirty subrepo dir on pull → stop with a clear message. Never export garbage.
- **Pushing to a public remote is irreversible.** Any code path that writes to a remote must
  run configured safety hooks (secret scan, transforms, excludes) *per exported commit* first.

## Stack & commands

- Rust (edition 2021), single binary crate, all code synchronous. MIT license.
- `cargo build` — compile (`target/debug/monosplice`). `cargo test` — everything: the
  in-module unit tests and the black-box e2e suite in `tests/`. `cargo clippy -- -D warnings`
  and `cargo fmt` must both be clean before anything lands.
- Dependencies are fixed: `clap` (+ `clap_complete`), `serde`, `serde_json`, `toml`,
  `globset`. **Do not add crates.** Everything else is std; git is always a subprocess.
- Config file is `monosplice.toml` (kebab-case keys, `deny_unknown_fields`), loaded and
  validated in `src/config.rs`. There is one filename and no config evaluation.
- Hooks (`scan`, `transform`, `rewrite-message`) are **shell commands** run via `sh -c`, not
  functions: `src/core/hooks.rs` for the contract, `src/core/filter.rs` for the order.
- Unit tests live in a `#[cfg(test)]` module inside the file they test; e2e tests are
  `tests/e2e_*.rs` over the shared harness in `tests/common/mod.rs`.

## Releasing & repo operations

- Repo: `github.com/jakequist/monosplice` (public; renamed from `monolith` — old URLs
  redirect). Local checkouts: `/home/jake/monosplice` (Linux), `/Users/jake/monosplice` (Mac).
- **Distribution: GitHub Releases is primary.** The release assets are five per-target
  tarballs, `monosplice-X.Y.Z-<target>.tar.gz` (x86_64/aarch64 linux-musl, x86_64/aarch64
  apple-darwin, x86_64-pc-windows-msvc), each holding one `monosplice` binary, plus
  `install.sh` — that is what the `curl … | sh` one-liner and `monosplice update` both read.
  npm still publishes, but the package in `npm/` is now a **shim** that downloads the same
  asset; `cargo install monosplice` is the third path.
- **The npm publish constraints are unchanged.** Package `monosplice`, published from CI via
  npm **trusted publishing** (OIDC). There is NO npm token anywhere and there must never be —
  do not add an `NPM_TOKEN` secret or `registry-url` to setup-node in release.yml (a
  placeholder token in .npmrc shadows the OIDC exchange). The trusted publisher registered
  on npmjs.com is exactly `release.yml` in this repo; renaming that file breaks publishing
  until the registration is updated.
- **To release:** bump `version` in **Cargo.toml AND npm/package.json** (they must match the
  tag) and the pinned string in `tests/e2e_help.rs` (`version_prints_the_crate_version`).
  CI runs clippy on the *latest stable* toolchain, so a new release can trip lints a stale
  local toolchain misses: `rustup update stable` first, or push main and wait for a green
  `ci.yml` before tagging. Then commit, `git tag vX.Y.Z`, push main + the tag. release.yml
  verifies tag == crate version, runs the full suite, cross-builds the five targets, creates the GitHub release
  with the tarballs, and publishes the npm shim. Nothing is ever published by hand.
- **If a release run fails partway, do NOT re-run it** — `gh release create` is not
  idempotent and npm refuses to republish a version. Instead: fix the problem, then
  `gh release delete vX.Y.Z --cleanup-tag --yes`, re-tag the fixed commit, push the tag.
  If it failed in `verify` (before any release exists), just delete the remote tag
  (`git push origin :refs/tags/vX.Y.Z`), re-tag, and push.
- The release job needs node ≥ 22 (`npm@latest` for OIDC dropped node 20 support) and
  workflow-level `permissions: id-token: write`.
- GitHub auth on the Linux box: `.env` (gitignored, never print/commit it) holds
  `GH_TOKEN`. Load with `set -a; source .env; set +a` for `gh`; pushes work via
  `git -c credential.helper='!f() { echo "username=jakequist"; echo "password=$GH_TOKEN"; }; f' push …`.
  On the Mac there is no `.env`: `gh` is already authenticated and `origin` is SSH, so a
  plain `git push` works.
- CI (`ci.yml`) builds and runs the full suite on every push/PR. Both workflows live in
  `.github/workflows/`.

## TDD — non-negotiable

This project is test-driven from day one. The workflow for every change:

1. **Write the failing test first.** For command behavior, that's an e2e scenario in
   `tests/e2e_*.rs` (see `docs/e2e-scenarios.md` for the backlog). For pure logic (trailer
   parsing, config validation, path filtering), a unit test in the `#[cfg(test)]` module of
   the file that owns the logic.
2. Run it, watch it fail for the right reason.
3. Implement the minimum to pass. Refactor with tests green.
4. A feature without a test does not exist. A bugfix starts with a regression test.

E2E conventions:
- Tests are **black-box**: they invoke the built binary via `env!("CARGO_BIN_EXE_monosplice")`
  and assert on exit codes, stdout, stderr and resulting git state. Never call crate
  internals from `tests/`.
- Remotes are **local bare repos** (`file://` URLs) in temp dirs. No network, ever.
- Determinism: fixed `GIT_AUTHOR_*`/`GIT_COMMITTER_*` env, `GIT_CONFIG_GLOBAL=/dev/null`,
  `GIT_CONFIG_SYSTEM=/dev/null`, fixed dates. Snapshot `git log --format=...` output freely.
- Shared harness lives in `tests/common/mod.rs` (`make_repo`, `make_bare_remote`,
  `run_monosplice`, `write_config`, `standard_fixture`, ...). Grow the harness instead of
  duplicating setup.

## How to work in this repo (agent orchestration)

The main session acts as **manager/architect**: it owns design decisions, task breakdown,
the invariants above, and final review. Low-level implementation is delegated to **Opus
subagents** via the Agent tool (`model: "opus"`).

- Slice work into agent-sized tasks: one command, one harness feature, one scenario batch.
- Each delegated task's prompt must include: the failing test(s) to make pass (or the test
  to write first), the relevant invariants from this file, and the files in scope.
- Subagents follow TDD too — instruct them to run the tests before and after.
- The manager reviews every diff against the invariants before it lands; don't rubber-stamp.
- Parallelize independent tasks (e.g., separate scenarios) in a single message.

## Code style

- Small modules with pure functions where possible; command modules stay thin and call into
  `src/core/`. No `unwrap()` on any path user input can reach — bubble a `Result` into the
  reporter instead.
- Error messages are written for the user at the terminal: say what happened, why monosplice
  stopped, and the exact command to run next.
- No comments that narrate the diff; comment only non-obvious constraints.
