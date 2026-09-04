# Monosplice 

### git subtrees for mere mortals

Monorepos are awesome.  Everything is synchronized and references each other in lock step.  But the world is harsh towards monorepos.  Sometimes we need to open source a package... other times we need to import a 3rd party vendor.   Monorepos break under these conditions.  

In the beginning, there were `git submodules`.  It was better than nothing, but oh so painful to use. 

Then came `git subtree`, which was better, but still too complicated to really use day-to-day.

Now, I give you `monosplice`.  A CLI that makes it easy'ish to push & pull subrepos from your monorepo. 

---

Keep everything in one monorepo, and splice any directory out as a real, standalone git
repo. `push` exports your commits to it; `pull` replays outside contributions back in —
commit by commit, authors and messages intact. Unlike `git subtree`, exports are
per-commit, filterable, and scannable before anything leaves.

<p align="center">
  <img src="docs/assets/splice-diagram.svg" alt="A monorepo file tree with packages/core, packages/client and vendor/lodash highlighted and synced bidirectionally to standalone repos core.git (private), client.git (public) and lodash.git (public)" width="880">
</p>

No submodules, no gitlinks, no special clone steps for contributors. The monorepo stays a
completely normal git repo; so does every repo spliced out of it — and as the diagram's
`core.git` shows, a spliced repo doesn't have to be public. Use it to open-source part of a
larger codebase, to vendor a third-party project you patch, or to maintain patches on a
fork — monosplice keeps a PR branch rebased on upstream for you.

## Install

One static binary, no runtime, no Node:

```sh
curl -fsSL https://github.com/jakequist/monosplice/releases/latest/download/install.sh | sh
```

The script detects your platform, downloads the matching release tarball, and drops the
binary in `/usr/local/bin` (or `~/.local/bin` if it can't write there). If you'd rather
build it yourself:

```sh
cargo install --git https://github.com/jakequist/monosplice
```

npm still works, for the muscle memory and for `package.json`-shaped CI — it is now a shim
that downloads the same binary:

```sh
npm install -g monosplice
```

Or take the tarball straight from
[Releases](https://github.com/jakequist/monosplice/releases) and unpack `monosplice`
wherever you like — the assets are named `monosplice-<version>-<target>.tar.gz`, one per
target triple. Full menu: [install options](docs/reference.md#install-options).

The only requirement is a system `git` (≥ 2.30) on your PATH. Once installed,
`monosplice update` replaces the binary in place with the latest release, and
`monosplice update --check` just tells you whether one is waiting.

Shell completion: `monosplice completion bash` (or `zsh`, `fish`) prints a script to
source.

## Quickstart

One-time setup, then pick the scenario that matches yours. All three use one command:
`attach` looks at which side already has content and does the right thing.

```sh
cd ~/code/my-monorepo
monosplice init          # writes monosplice.toml
```

The scaffold is a commented-out `[[subrepos]]` block; `attach` fills it in for you — see
[configuration](docs/reference.md#configuration).

### I have a monorepo and want to extract a subrepo

```bash
# Create a new repo at github.com:acme/core.git
$ monosplice attach ./core git@github.com:acme/core.git
```

That's it. `core/` is now the root of a real repo, and your monorepo didn't even notice.

### I have a monorepo and want to import an external repo

```sh
monosplice attach ./packages/auth git@github.com:acme/auth.git
```

`packages/auth/` doesn't exist yet, so attach copies the repo's current tree there in a
single commit and records which remote commit it came from. You're in sync immediately — no
need to replay the remote's history (though `--import-history` will, if you want it in your log).
Edge cases — the folder already has content, the trees differ — are covered in
[connecting a repo that already exists](docs/reference.md#connecting-a-repo-that-already-exists).

### I have a vendor repo and want to splice it

```sh
monosplice attach ./vendor/lodash git@github.com:lodash/lodash.git
```

The same move as importing — the `vendor/` prefix is just convention. From then on it's a
normal directory: patch it in ordinary commits, and `monosplice pull lodash` three-way
merges upstream updates underneath your patches. To send patches *back* to a project you
can't push to, see the [fork workflow](docs/reference.md#pushing-patches-back-upstream-fork-workflow).

### Life continues as normal

Whatever you attached, your monorepo behaves as if the subrepos are simple files.  You manage the subrepos via the `monosplice` CLI. 

```sh
git commit -am "feat(core): add the greeter"
git commit -am "chore(website): copy tweaks"   # touches nothing in core/, never exported

monosplice status   # core: 1 to push
monosplice push     # exports the core/ commit, and only that one
```

To fetch the latest code from an upstream subrepo: 

```sh
monosplice pull     # replays it into core/, original author preserved
monosplice sync     # pull then push, in one go
```

When you need more — `exclude` globs, secret-scan and tree-transform hooks, the fork
workflow — it's all in [configuration & hooks](docs/reference.md#configuration).

## Commands

| Command | What it does |
| --- | --- |
| `monosplice init` | Write a starter `monosplice.toml`. |
| `monosplice push [subrepo]` | Export new monorepo commits to the standalone repo(s). `--dry-run` lists them and writes nothing. |
| `monosplice pull [subrepo]` | Import new standalone-repo commits into the monorepo. `--dry-run` previews, `--continue` resumes after a conflict, `--abort` throws it away. |
| `monosplice sync [subrepo]` | Pull, then push — converge both sides. `--continue` finishes a sync that stopped on a conflict. |
| `monosplice status [--json] [--check] [--offline]` | Per-subrepo "N to push, M to pull", or "in sync". `--check` exits 1 unless everything is converged; `--offline` fetches nothing. |
| `monosplice attach <folder> [git-url]` | Connect `<folder>` to a repo — creates the config entry and makes first contact. See Quickstart. |
| `monosplice detach <subrepo>` | Stop tracking a subrepo: removes the config entry, keeps the folder and all of its history. |
| `monosplice doctor [--json]` | Verify the derived sync state against reality; non-zero exit on problems. |
| `monosplice tag <subrepo> <tag>` | Tag the standalone repo at the commit matching monorepo HEAD. |
| `monosplice completion <shell>` | Print a completion script for bash, zsh or fish. |
| `monosplice update` | Replace the installed binary with the latest release (`--check` only reports). |

Full flags and edge-case behaviour: [docs/reference.md](docs/reference.md).

## How it works

No magic, no daemon, no lock-in.

**Two histories, one mapping.** The monorepo and each standalone repo have independent
histories; monosplice replays commits between them and records the correspondence in commit
trailers (the `Key: value` lines git keeps at the end of a commit message) — exports carry
`Monosplice-Source: <monorepo-sha>`, imports carry `Monosplice-Origin: <standalone-sha>`. Each
export is built with git plumbing (`ls-tree`, `mktree`, `commit-tree`) and preserves the
original author and dates. The remote ref is written exactly once — after every commit and
every hook has succeeded. Commits that touch nothing exportable produce no commit on the
other side, and a squash-merged PR converges by itself: the squashed commit imports as the
new anchor, and nothing gets re-published or ping-pongs.

**There is no persistent state file.** Nothing on disk records "where we got to" — the only
thing ever written under `.git/monosplice/` is a transient conflict sequencer, exactly like
git's own `.git/rebase-merge`. Every run re-derives the sync point from the trailers, and a
commit counts as synced when the sync point descends from it — which is why attaching a
200-commit repo with a single snapshot commit reports "in sync", not "200 to pull". A fresh
clone on a new machine can push, pull and sync immediately: no cache to invalidate, no
lockfile to conflict on. And when the picture doesn't add up (shallow clone, rewritten
history), monosplice stops and says so rather than guessing. A rebase is the one rewrite it
settles by itself: if the anchor sha is gone but a commit still on your branch publishes
exactly the tree the standalone repo already carries, `push` adopts that commit as the anchor,
prints `recovered anchor: <old> → <new>`, and exports only the work after it — the export was
correct, only the sha was stale. Anything that actually changed the published tree still stops.
Clones made *after* someone else's rebase inherit the other half of that story — an old public
commit naming a sha nobody has any more. Validation stops at the newest anchor that resolves,
so a dead trailer behind a live one is reported as history and blocks nothing; only a dead
anchor with nothing readable above it (a shallow clone, the wrong remote) refuses. The same
rule runs in the import direction: re-baseline a subrepo onto a brand-new repository and the old
`Monosplice-Origin` trailers below the live anchor become history too — reported, never fatal.

**Hooks run before anything leaves.** `exclude` globs filter files out of the export, and
three per-commit shell hooks — `scan`, `transform`, `rewrite-message` — inspect or rewrite
each outgoing tree; a non-zero exit aborts the whole push with nothing published. A secret
scan runs against *every* exported commit, so a key that was added and deleted still blocks
the push. See [configuration & hooks](docs/reference.md#configuration).

**Conflicts are just merges.** Imports apply with `git apply --3way`, so concurrent edits to
different lines merge silently. A real conflict leaves standard markers; you resolve,
`git add`, `monosplice pull --continue` — or `monosplice pull --abort` to pretend the whole
thing never happened. A resolution you keep is re-exported, so neither side loses it.
Details: [the conflict flow](docs/reference.md#the-conflict-flow).

## Configuration

`monosplice.toml` at the root of your monorepo, one `[[subrepos]]` block per spliced
directory:

```toml
[[subrepos]]
path = "core"                                # directory in the monorepo
remote = "git@github.com:you/core.git"       # the standalone repo
branch = "main"                              # optional, default "main"
exclude = ["INTERNAL.md", "**/*.internal.ts"]  # optional globs, relative to core/

[[subrepos]]
path = "vendor/lodash"
remote = "git@github.com:you/lodash.git"     # your fork — the push destination
upstream = "git@github.com:lodash/lodash.git"  # where updates come from
push-branch = "monosplice/patches"           # the PR branch monosplice rebuilds
```

Hooks are shell commands. `scan` runs in the outgoing tree, and a non-zero exit stops the
push before a single byte reaches the remote:

```toml
[[subrepos]]
path = "core"
remote = "git@github.com:you/core.git"
scan = "if grep -rIqE 'AKIA[0-9A-Z]{16}' .; then echo 'AWS key in the outgoing tree' >&2; exit 1; fi"
```

Every key, and the exact contract the hooks run under, is in
[configuration & hooks](docs/reference.md#configuration). Coming from a
`monosplice.config.ts`? [Migrating from monosplice.config.ts/js](docs/reference.md#migrating-from-monospliceconfigtsjs)
maps it key by key.

## Compared to the alternatives

| | git submodule | git subtree | git-subrepo | monosplice |
| --- | --- | --- | --- | --- |
| Files in the monorepo | pointer, not content | real files | real files | real files |
| Contributor setup | `submodule update --init`, forever | none | none | none |
| Export granularity | n/a (same repo) | squash or graft | squashed per push | per commit |
| Contributions back in | manual, by hand | `subtree pull` (merge noise) | `subrepo pull` (squash) | yes, `monosplice pull` with 3-way merge |
| Secret scan / tree transform | no | no | no | yes (shell hooks) |
| Where the mapping lives | gitlink shas | subtree merge commit messages | `.gitrepo` file committed in your tree | commit trailers, re-derived every run |
| Runtime | git | git | bash | one static binary + git |
| Scope | vendoring dependencies | grafting a directory in/out | one dir ↔ one repo | one monorepo publishing a handful of directories |

Short version: submodules make the *contributor* pay for your publishing strategy.
`git subtree` moves directories but has no excludes, no transforms, no scan that can block a
push, and its history gets noisy fast. `git-subrepo` is the closest cousin — same
one-dir↔one-repo shape — but it squashes on both sides and keeps its mapping in a
`.gitrepo` state file committed into your tree, where monosplice keeps per-commit fidelity
and derives the mapping from trailers on every run. `josh` solves a different problem
beautifully — serving many filtered views of one big monorepo — at the price of running a
proxy server everyone clones through. Copybara does all of this and more, and is far more
mature — but it's a Java/Bazel deployment with Starlark configs. Heavy, if all you have is
one repo and one `core/` directory. monosplice aims at that smaller case with a config file
you can read in ten seconds.

## Limitations

Things monosplice won't do, listed here so you don't find out the hard way:

- **One branch per subrepo.** monosplice syncs the configured branch (default `main`) and
  nothing else; feature-branch export is on the roadmap.
- **Exported commits are watermarked.** Every export carries a
  `Monosplice-Source: <monorepo-sha>` trailer. That trailer *is* the sync mapping, so
  `rewrite-message` runs before it is appended and cannot strip it — private-monorepo SHAs
  appear in standalone-repo history, permanently. They reveal nothing but 40 hex characters,
  but know it's there before you publish.
- **No shallow clones.** Sync state is re-derived by walking history, so a shallow monorepo
  clone stops with an error rather than guessing.
- **`status` talks to the network by default.** Re-deriving state is a couple of `git log`
  scans per subrepo — cheap. Fetching each remote is what you actually wait on;
  `monosplice status --offline` skips it and measures against the last fetch instead.
- **Hooks are shell commands, and they want a POSIX shell.** They run via `sh -c`, so the
  hook you write is the hook that runs — but a JavaScript function in a config file is no
  longer a thing monosplice can execute. Call your script from the shell command instead.

## Going deeper

- [Connecting a repo that already exists](docs/reference.md#connecting-a-repo-that-already-exists)
- [Vendoring a third-party project](docs/reference.md#vendoring-a-third-party-project)
- [Fork workflow — PRs back upstream](docs/reference.md#pushing-patches-back-upstream-fork-workflow)
- [Configuration & hooks](docs/reference.md#configuration)
- [Migrating from monosplice.config.ts/js](docs/reference.md#migrating-from-monospliceconfigtsjs)
- [The conflict flow](docs/reference.md#the-conflict-flow)
- [Install options & releasing](docs/reference.md#install-options)

## Development

```sh
cargo build           # debug binary at target/debug/monosplice
cargo test            # unit tests (in-module) + the black-box e2e suite
cargo clippy -- -D warnings
cargo fmt
```

The project is test-driven: new behaviour starts as a scenario in
[`docs/e2e-scenarios.md`](docs/e2e-scenarios.md), gets a failing black-box test in
`tests/e2e_*.rs` (or a unit test beside the code it covers for pure logic), then the
implementation. E2E tests invoke the built binary and assert on exit codes, stdout and git
state; "remotes" are local bare repos, so the suite never touches the network. Releasing is
[tag-driven](docs/reference.md#releasing).

## Roadmap

Not built yet, in rough order of usefulness:

- **Branch export** — sync branches other than the configured one, so feature branches and release branches can be published too.
- **A GitHub Action** — run `monosplice sync` (or at least `monosplice status`) in CI on a schedule.

## License

MIT.
