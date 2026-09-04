# monosplice reference

The [README](../README.md) covers the quickstart and the core model. This is the detailed
reference for everything else: connecting repos that already exist, vendoring and the fork
workflow, the full configuration surface, conflicts, migrating off the old JavaScript
config, and releasing.

## Connecting a repo that already exists

`monosplice attach` is the one command for first contact, whichever side already has
something. First contact itself is detected, never configured: monosplice looks at two things
— whether the folder has committed content, and whether the remote branch exists — and there
is exactly one right move for each combination.

```sh
monosplice attach core git@github.com:you/core.git   # folder not in your config yet
monosplice attach core                               # folder already in your config
```

With a URL, `attach` writes the `[[subrepos]]` entry for `<folder>` into your
`monosplice.toml` first. Without one, `<folder>` must already match a configured
subrepo — by path or by name — and nothing is written to the config at all; only first
contact is made. Either way the move is the same:

| `path/` in the monorepo | remote branch | What happens |
| --- | --- | --- |
| empty / absent | has history | Materializes the remote's HEAD tree at `path/` in **one** monorepo commit (`Adopt <name> from …`, carrying `Monosplice-Origin`). `--import-history` replays every commit from the standalone repo instead, authors and messages preserved. |
| has content | has history | Only if the two trees already match — that records the baseline as an empty commit. Otherwise monosplice lists the differing paths and stops; `--theirs` replaces `path/` with the remote tree in one commit. |
| has content | empty | The first publish, confirmation-gated: a prompt at a terminal, `--yes` in scripts. Publishes the current tree as one `Initial import of <name>` commit; `--export-history` replays every monorepo commit that touched the directory instead. |
| empty / absent | empty | Nothing exists yet. Commit something, or point the URL at a repo that has content. |

```sh
# a repo with 200 commits of its own history, no core/ in the monorepo yet
monosplice attach core git@github.com:you/core.git             # one commit: "Adopt core from …@ 9f2c1ab0e4"
monosplice attach core git@github.com:you/core.git --import-history   # …or replay all 200 into core/
```

When the folder is new, the config entry and the tree land in the **same** commit — the
anchor and the entry that gives it meaning belong together. The two exceptions commit the
entry on its own first, because what follows cannot share a commit with it: `--import-history`
(each replayed commit is its own) and a first publish (which asks before writing to the
remote, and the entry must survive a "no").

Either way the anchor commit carries `Monosplice-Origin: <pub-sha>`, which is what makes
`status` say "in sync" immediately: the remote history is reflected by ancestry, not by
importing it commit by commit. Everything before it stays in your monorepo history and is
never exported — the next `push` publishes only genuinely new work, parented on the remote's
existing head.

`--name` defaults to the last segment of `<folder>`, `--branch` to `main`; on an
already-configured folder both are refused rather than silently ignored, and so is a URL that
disagrees with the configured `remote`. Every refusal — name or path already configured,
nesting, a dirty tree, a pull in progress, an unreachable URL, differing trees — leaves the
config byte-identical and makes no commit.

`push` and `pull` refuse to guess. Pointed at a remote whose history is unrelated to the
monorepo, both stop and tell you to run `attach`; run `attach` on a pair that is already
connected by trailers and it stops too.

### Can you actually push there?

Attaching proves you can *read* the remote. Writing to it needs rights nothing so far has
exercised, so after a successful attach to a remote that has history monosplice runs a
harmless `git push --dry-run` of the remote's own head back at it. If that is refused, the
attach still stands (exit 0 — the anchor commit is real and `pull` works), and monosplice
prints an advisory naming the fork setup to use instead. It never blocks, and it is skipped
where it would be meaningless: with `--fork`, or when the remote was empty and the first
publish proved write access by doing it.

## Disconnecting a subrepo

`monosplice detach <subrepo>` is the reverse of attach's config write, and *only* that:

```sh
monosplice detach core
# ✓ detached core — /repo/monosplice.toml no longer tracks git@github.com:you/core.git
#   core/ is kept exactly as it is, and every commit stays in your monorepo history.
#   The Monosplice trailers on those commits are inert now: nothing is pushed or pulled for core any more.
#   To connect it again later:
#     monosplice attach core git@github.com:you/core.git
```

The folder stays, every commit stays, and the `Monosplice-Source`/`Monosplice-Origin` trailers
on past commits simply go inert — nothing reads them once no entry names that subrepo. The
config edit is committed on its own (`Detach <name>: stop tracking <url>`), and the output
prints the `monosplice attach <path> <url>` that connects it again later, with the URL it was
actually tracking (plus `--fork` when the entry was triangular).

It never contacts the network — there is nothing to tell the remote — and it refuses, leaving
the config byte-identical and making no commit, on an unknown subrepo, on a subrepo whose pull
is sitting unfinished, and on a dirty working tree or a dirty index (it commits the index, so
it insists on the same clean tree `attach` does).

The removal is textual, then verified: monosplice cuts the matching `[[subrepos]]` block —
from its header line to the next `[`-header or the end of the file — reloads the config
through the real loader, and checks that the named subrepo is gone *and* that every other one
still resolves exactly as it did. It refuses outright when zero or several blocks match, or
when a block's identity cannot be read from a plain single-line TOML string (a `name` or
`path` written as a multiline string, or as anything that is not a string). Then the original
bytes go back byte-for-byte, no commit is made, and monosplice tells you which entry to delete
by hand.

## Vendoring a third-party project

The same command covers a third-party repo you want *inside* your monorepo — tracked,
patchable, and still able to take upstream updates. The `vendor/` prefix is pure convention:

```sh
monosplice attach vendor/lodash git@github.com:lodash/lodash.git
# ✓ attached lodash at vendor/lodash (tracking git@github.com:lodash/lodash.git#main) @ 9f2c1ab0e4
#   vendor/lodash/ and the remote are now in sync; push and pull as usual.
```

One command, one commit: the entry goes into your `monosplice.toml`, lodash's current
tree is materialized at `vendor/lodash/`, and both are committed **together** with a
`Monosplice-Origin` trailer anchoring the pair. The subrepo name defaults to the last path
segment (`lodash`); `--name` and `--branch` override the defaults.

From then on it is a normal subrepo. Patch it like any other directory in your monorepo:

```sh
git commit -am "fix(lodash): guard against a null prototype"
```

and take upstream updates whenever you like:

```sh
monosplice pull lodash    # replays new upstream commits into vendor/lodash/
```

Your patch and upstream's commits are three-way merged, so an upstream change to a different
file lands silently and your patch survives. When upstream edits the same lines you did, you
get the standard conflict flow — markers in `vendor/lodash/`, resolve, `git add`,
`monosplice pull --continue` — and your resolution is preserved.

With only `remote` set it is both the pull source and the push destination, so `monosplice push
lodash` would try to write to lodash's own repository. Almost nobody has permission to do
that. Point monosplice at a fork instead — see the next section.

## Pushing patches back upstream (fork workflow)

Set `upstream` and the subrepo becomes triangular: monosplice **pulls from upstream** and
**pushes to your fork**, which is exactly the shape a pull request wants.

```toml
[[subrepos]]
path = "vendor/lodash"
remote = "git@github.com:you/lodash.git"        # your fork — the push destination
upstream = "git@github.com:lodash/lodash.git"   # where updates come from
branch = "main"                                 # branch tracked on upstream
push-branch = "monosplice/patches"              # optional, defaults to `branch`
```

`monosplice attach vendor/lodash <upstream-url> --fork <fork-url>` writes that entry for you,
then attaches against **upstream** — the tree, the anchor and every later sync decision come
from there, and the fork is only ever written to by `push`. `--fork` must differ from the URL
you are attaching, and it only applies to a folder that is not configured yet: on an existing
entry monosplice tells you to add `upstream` to the config instead of guessing which of the
two remotes you meant to keep.

The loop:

```sh
git commit -am "fix(lodash): guard against a null prototype"   # patch it in the monorepo
monosplice push lodash    # rebuilds you/lodash's monosplice/patches = upstream main + your patches
# open the PR from that branch
monosplice pull lodash    # takes upstream updates whenever you like
```

The fork's `push-branch` is a **derived artifact** monosplice owns: every push rebuilds it as the
current upstream head plus your patches, replayed in order, and writes it with
`--force-with-lease` so a branch somebody else moved is never clobbered silently. Exports are
sha-deterministic, so rebuilding an unchanged chain produces the identical branch and monosplice
reports "up to date" instead of pushing. Upstream itself is **never** written to — not a
branch, not a tag. (`monosplice tag` refuses on a triangular subrepo for the same reason.)

Once upstream advances, `monosplice sync` imports their commits under your patches and rebuilds
the fork branch on the new upstream head, so the PR stays applicable.

When the PR is merged, everything converges by itself:

- **Merged or rebased in:** your exported commits arrive in upstream carrying their
  `Monosplice-Source` trailers, so `pull` skips them, the anchors move forward, and `push` says
  up to date.
- **Squash-merged:** upstream gets one new commit with your tree and none of your trailers.
  `pull` imports it (usually as an empty commit — the content is already there), and because
  that import reproduces the upstream tip exactly, it becomes the newest export anchor and the
  old patch commits fall out of the scan range. `push` is up to date, with nothing
  re-published and no ping-pong.

`status` measures ahead/behind against upstream, and says `N to push (awaiting upstream merge)`
once your fork branch already carries the commits — the ball is in the maintainer's court, not
yours. `doctor` fetches both sides and reports them separately, so an unreachable fork never
looks like a broken upstream.

Two notes on the config edit. An array of tables can only grow at the bottom, so monosplice
appends the rendered `[[subrepos]]` block at the end of the file (after one blank line) and
then **reloads your config through the real loader**, checking that the new entry resolves to
exactly what it wrote; if it does not — a duplicate name or path, a nesting violation, a file
that no longer parses — it restores the original bytes byte-for-byte, makes no commit, and
prints the entry for you to paste in yourself, naming the `monosplice attach <folder>` that
finishes the job once you have. And `attach` refuses to start unless the working tree is
clean, because it commits the index.

## Configuration

`monosplice.toml` sits at the root of your monorepo — that is what `monosplice init` writes.
Every command walks up from the current directory looking for it, and the directory holding it
is the monorepo root. There is exactly one filename; nothing is loaded, compiled or evaluated,
so there is no build step, no plugin, and no way for a config file to do anything but describe
subrepos.

The scaffold `init` writes is entirely commented out — an array of tables has to have somewhere
to grow, and `attach` appends to the bottom of the file:

```toml
# Monosplice configuration.
# Docs: https://github.com/jakequist/monosplice
#
# Each subrepo is one [[subrepos]] block:
#
# [[subrepos]]
# path = "packages/my-lib"
# remote = "git@github.com:me/my-lib.git"
# branch = "main"
# exclude = []
```

A filled-in config is one `[[subrepos]]` block per spliced directory:

```toml
[[subrepos]]
name = "core"                                   # optional, defaults to the last path segment
path = "core"                                   # directory in the monorepo; nested paths are fine
remote = "git@github.com:you/core.git"          # any git URL
branch = "main"                                 # optional, default "main"
exclude = ["INTERNAL.md", "**/*.internal.ts"]   # optional globs, relative to the subrepo dir
```

### The full configuration surface

| Key | Type | Required | Notes |
| --- | --- | --- | --- |
| `path` | string | yes | Directory inside the monorepo, relative to the config file. `packages/lib` is fine; leading `./` and surrounding slashes are normalized away. Cannot be the repo root, cannot contain `.` or `..` segments, and two subrepos may not nest inside one another. |
| `remote` | string | yes | Git URL of the standalone repository. With `upstream` set, this is your fork: the push destination, and the only repo monosplice writes to. |
| `upstream` | string | no | Git URL to pull from when it differs from the one you push to (fork workflow). Every sync decision — imports, anchors, ahead/behind — is made against it. Must differ from `remote`. |
| `name` | string | no | The handle you type (`monosplice push core`). Defaults to the last segment of `path`. Must be unique across the config. |
| `branch` | string | no | Branch synced on both sides. Default `main`. With `upstream` set, the branch tracked on upstream. |
| `push-branch` | string | no | Branch monosplice rebuilds on your fork. Defaults to `branch`. Requires `upstream` — without one there is no fork/upstream split, and the config is refused. |
| `exclude` | array of strings | no | Globs relative to the subrepo directory, matched against every file before export. `*` stays inside one path component, `**` spans components, and dotfiles are matched by wildcards. |
| `rewrite-message` | string | no | Shell command that rewrites outgoing commit messages. |
| `transform` | string | no | Shell command that may mutate the outgoing tree. |
| `scan` | string | no | Shell command that inspects the outgoing tree; a non-zero exit blocks the push. |

Unknown keys are errors, not decoration: a typo (`brunch`) or a leftover camelCase spelling
(`pushBranch`) stops the command instead of being silently ignored. So do empty strings, an
`upstream` equal to `remote`, duplicate names or paths, and nested paths. Every refusal reads
the same way, naming the file and the offending field:

```console
$ monosplice status
Error: Invalid config at /repo/monosplice.toml:
  subrepos[0].push-branch — push-branch requires upstream: it names the branch monosplice pushes on your fork, and without `upstream` there is no fork/upstream split. Drop push-branch, or set `upstream` to the repository you pull from.
```

A config with no `[[subrepos]]` at all is valid — it means "nothing attached yet", which is
exactly the state `init` leaves you in.

### Hooks

Hooks are shell commands. All three run **per exported commit**, against the tree that commit
would publish, *before* anything is written to the remote; a non-zero exit aborts the whole
push with nothing published. Each is run with `sh -c "<your command>"`, with three environment
variables set:

| Variable | Value |
| --- | --- |
| `MONOSPLICE_SUBREPO` | the subrepo's name, as configured |
| `MONOSPLICE_MONO_SHA` | the monorepo commit being exported (full sha) |
| `MONOSPLICE_MESSAGE_FILE` | path to a temporary file holding that commit's original, pre-rewrite message |

**`scan` and `transform` run inside the outgoing tree.** monosplice materializes the
post-exclude tree into a temporary directory — file contents with the exec bit git recorded,
symlinks as real symlinks — and makes that directory the hook's working directory. So `ls`
shows the files the standalone repo would receive, at paths relative to the *subrepo* root
(`src/index.ts`, not `core/src/index.ts`), and nothing else from your monorepo is reachable by
a relative path. Submodule (gitlink) entries are the exception: they have no content to write,
so they are passed through untouched rather than materialized.

`scan` runs **first**, before `transform` gets to rewrite anything and before a single git
object is created. A non-zero exit stops the export there, and the hook's stderr becomes the
error detail:

```console
$ monosplice push
Error: core: scan hook rejected core commit 4a91c2f0b1e83c7d5a0f9b2e6c1d4a8f0b3e7c92: AWS access key id in fixtures/dump.sql
Nothing was pushed to git@github.com:you/core.git.
```

`transform` may add, modify or delete files in that directory. On exit 0 the directory is
re-hashed and *that* is the tree which exports — modes come from the filesystem (exec bit →
`100755`, symlink → `120000`, otherwise `100644`), and files a transform added, dotfiles
included, are part of the export. Your monorepo working tree and index are never touched; only
the object database is written.

`rewrite-message` is the odd one out: it shapes the commit, not the tree, so it runs from the
monorepo root with the original message on **stdin** and the rewritten message expected on
**stdout**. It runs *before* the `Monosplice-Source` trailer is appended, so you cannot
accidentally strip it.

Worked examples, one of each:

```toml
[[subrepos]]
path = "core"
remote = "git@github.com:you/core.git"
exclude = ["**/*.internal.ts", "fixtures/prod-dump.sql"]

# Refuse to publish anything that looks like a credential. `-I` skips binaries; the message on
# stderr is what the error shows.
scan = "if grep -rIqE 'AKIA[0-9A-Z]{16}|-----BEGIN [A-Z ]*PRIVATE KEY-----' .; then echo 'credential in the outgoing tree' >&2; exit 1; fi"

# Swap the internal README for the public one, in the exported tree only.
transform = "if [ -f README.public.md ]; then mv README.public.md README.md; fi"

# Prefix every published subject with the subrepo name, and drop internal ticket trailers.
rewrite-message = "sed -e \"1s/^/[$MONOSPLICE_SUBREPO] /\" -e '/^Internal-Ticket: /d'"
```

Because `scan` runs against every commit being exported — not just the final tree — a secret
that was added and later deleted still blocks the push, which is the correct behaviour:
publishing that history would publish the secret.

Two rules worth internalizing:

- **Determinism is your job.** monosplice replays the same commits from the same base and
  expects the same shas; a `transform` that stamps a build date or a random id will churn the
  standalone repo (and, in the fork workflow, force-push a new branch) on every run.
- **Hooks gate the write, so they do not run on `--dry-run`.** See
  [previewing a run](#previewing-a-run-and-working-offline).

Hooks are the one capability the Rust rewrite changed: they used to be JavaScript functions in
a `monosplice.config.ts`. See the next section for the translation.

## Migrating from monosplice.config.ts/js

monosplice used to be a Node CLI configured by a JavaScript or TypeScript module. It is now a
single binary configured by `monosplice.toml`, and nothing loads `monosplice.config.ts`,
`.mts`, `.js`, `.mjs` or `.cjs` any more. Finding one of those with no `monosplice.toml`
beside it is not "no config" — it is an un-migrated repo, and every command stops and says so
rather than walking past it and reporting the wrong root:

```console
$ monosplice status
Error: Found /repo/monosplice.config.ts but no monosplice.toml beside it.
monosplice is now configured by monosplice.toml, not by JavaScript or TypeScript config files. Nothing was changed. Translate that file into a monosplice.toml in /repo — the migration section of docs/reference.md shows the key-by-key equivalent — then run the command again.
```

While both files exist, the TOML wins silently: you are mid-migration, and the old file is
inert. Delete it when you are done.

### Key by key

| `monosplice.config.ts` | `monosplice.toml` |
| --- | --- |
| `export default {subrepos: [...]}` | one `[[subrepos]]` block per entry (no top-level wrapper to write) |
| `defineConfig()` / the `@type` JSDoc | nothing — the loader validates the file and names the offending field |
| `name: 'core'` | `name = "core"` |
| `path: 'core'` | `path = "core"` |
| `remote: 'git@…'` | `remote = "git@…"` |
| `upstream: 'git@…'` | `upstream = "git@…"` |
| `branch: 'main'` | `branch = "main"` |
| `pushBranch: 'patches'` | `push-branch = "patches"` |
| `exclude: ['**/*.secret']` | `exclude = ["**/*.secret"]` |
| `rewriteMessage(message, ctx) {…}` | `rewrite-message = "<shell command>"` |
| `transform(files, ctx) {…}` | `transform = "<shell command>"` |
| `scan(files, ctx) {…}` | `scan = "<shell command>"` |

The only spelling change is the case convention: multi-word keys are kebab-case now, so
`pushBranch` becomes `push-branch`. Every other key keeps its name, and every default,
refusal and message is the same. A camelCase leftover is not silently ignored — it is an
unknown field, and the command stops.

### Before and after

```ts
// monosplice.config.ts — delete this file when you are done
import {defineConfig} from 'monosplice'

export default defineConfig({
  subrepos: [
    {
      name: 'core',
      path: 'packages/core',
      remote: 'git@github.com:you/core.git',
      branch: 'main',
      exclude: ['INTERNAL.md', '**/*.internal.ts'],
      scan(files) {
        for (const [file, {data}] of files) {
          if (/\bAKIA[0-9A-Z]{16}\b/.test(data.toString('utf8'))) {
            throw new Error(`AWS access key id in ${file}`)
          }
        }
      },
    },
    {
      path: 'vendor/lodash',
      remote: 'git@github.com:you/lodash.git',
      upstream: 'git@github.com:lodash/lodash.git',
      pushBranch: 'monosplice/patches',
    },
  ],
})
```

```toml
# monosplice.toml
[[subrepos]]
name = "core"
path = "packages/core"
remote = "git@github.com:you/core.git"
branch = "main"
exclude = ["INTERNAL.md", "**/*.internal.ts"]
scan = "if grep -rIqE 'AKIA[0-9A-Z]{16}' .; then echo 'AWS access key id in the outgoing tree' >&2; exit 1; fi"

[[subrepos]]
path = "vendor/lodash"
remote = "git@github.com:you/lodash.git"
upstream = "git@github.com:lodash/lodash.git"
push-branch = "monosplice/patches"
```

### What to do about function hooks

A binary cannot call a JavaScript closure, so the three hooks became shell commands. The
translation is mechanical once you know where each input went:

| In the function hook | In the shell hook |
| --- | --- |
| `files` (a `Map` of path → `{mode, data}`) | the working directory itself — the outgoing tree, materialized on disk |
| `ctx.subrepo` | `$MONOSPLICE_SUBREPO` |
| `ctx.monoSha` | `$MONOSPLICE_MONO_SHA` |
| `ctx.message` (and the `message` argument) | stdin for `rewrite-message`; `$MONOSPLICE_MESSAGE_FILE` for `scan`/`transform` |
| returning a string from `rewriteMessage` | writing it to stdout |
| mutating or returning `files` from `transform` | editing files in the working directory |
| `throw new Error(detail)` | writing `detail` to stderr and exiting non-zero |

Most scans and transforms get *shorter*: `grep -rIq`, `sed -i`, `rm`, and a `mv` are usually
the whole hook. When the logic is genuinely worth keeping in TypeScript, keep it — as a
script the shell hook invokes:

```toml
scan = "node scripts/scan.mjs"
```

with one caveat about paths, which follows directly from the cwd rule above:

- `scan` and `transform` run **inside the materialized outgoing tree**, so
  `node scripts/scan.mjs` finds the script only when `scripts/scan.mjs` is part of the subrepo
  being exported (it ships with the published repo, which is often exactly what you want). A
  script that lives elsewhere in the monorepo needs an absolute path — `node
  /repo/scripts/scan.mjs` — or to be on `PATH`.
- `rewrite-message` runs from the **monorepo root**, so a relative path works as written.

Your script reads the tree from its own working directory instead of a `FileMap`, and signals
rejection by exiting non-zero with a message on stderr instead of throwing. Everything else —
when it runs, what it can block, what it may rewrite — is unchanged.

## The conflict flow

Imports are the only operation that touches your working tree, because a conflicting import is a merge only you can resolve.

```console
$ monosplice pull
Error: core: importing 4a91c2f0b1 conflicts with local changes.
Conflicted files:
  core/src/index.ts
Edit each file to resolve the markers, `git add` it, then run:
  monosplice pull --continue
To abandon the import instead, restoring the monorepo to its pre-pull state:
  monosplice pull --abort
```

Each incoming commit is applied with `git apply --3way --index`, so non-conflicting concurrent
edits merge silently. On a real conflict, monosplice leaves standard conflict markers in your
working tree and writes a sequencer file under `.git/monosplice/` — a transient record of which
commit we were on, what is left, where the run started and what it has committed so far,
exactly like `.git/rebase-merge`. It is never committed and never part of your project.

You resolve, `git add`, and run `monosplice pull --continue`. The import lands as a monorepo
commit carrying `Monosplice-Origin`, and the remaining commits replay on top. A conflict stops
the whole run even with several subrepos configured, because only one sequencer can exist at a
time.

A conflict during `monosplice sync` names `monosplice sync --continue` instead, and that is the one to run: it finishes the interrupted import exactly as `pull --continue` does and then runs the push phase the interrupted run never reached — for **every** subrepo, since one that is already converged simply reports "up to date". `monosplice pull --abort` still abandons the import whichever command started it; there is one sequencer, and throwing it away is the same act either way.

### Aborting

`monosplice pull --abort` throws the interrupted import away. It restores the subrepo directory and the index to the tree they had before the pull started, deletes the sequencer, and rewinds the commits this pull run had already imported — but only when the sequencer can *prove* they are its own: it recorded the pre-pull HEAD and the sha of every commit it created, and it rewinds only if the commits between the two are exactly that list and nothing else. If you committed something yourself after the conflict, that proof fails, so abort undoes only the conflicted step, keeps the rest, and prints the pre-pull sha so you can decide for yourself.

Nothing outside the subrepo directory is ever touched — unstaged edits and untracked files elsewhere in the monorepo survive an abort untouched, which a plain `git reset --hard` would not manage. Untracked files *under* the subrepo path do not: `pull` refuses to start unless that directory is pristine, so anything untracked there was created by the import being abandoned.

Aborting with no pull in progress is an error, and so is combining `--abort` with `--continue`.

Then comes the subtle part, and it is deliberate: your resolution is **re-exported** on the next push. A pure import reproduces the remote tip's tree exactly, so the tree-equality check drops it and nothing is published (no ping-pong). But a *conflicted* import is a genuine merge of monorepo and external edits — its tree differs from the remote tip — so it must go out, or the standalone repo would silently lose your resolution. That is the rule that keeps "the exported tree equals the filtered monorepo tree" true after every push.

## Previewing a run, and working offline

`--dry-run` on `push` and `pull` prints exactly what would move and writes nothing — no remote
ref, no monorepo commit, no working-tree or index change:

```console
$ monosplice push --dry-run
core: 2 to push (dry run — nothing written)
  4a91c2f0b1 feat(core): add the greeter
  9f2c1ab0e4 fix(core): guard the empty case
```

Nothing pending prints the up-to-date line, and either way the exit code is 0. The plan comes
from the same candidate scan `push` and `status` share, so it is not a separate code path that
can drift.

One deliberate gap: **`scan` and `transform` hooks do not run on a dry run.** They are the gate
on writing to a remote, and a dry run does not write — so the list is what would be *attempted*,
and a commit a hook would reject still appears in it. The real push is still gated: a hook that
exits non-zero aborts it with nothing published. `pull --dry-run` likewise skips the
clean-working-tree check a real pull insists on, since that check exists to protect a write.

`monosplice status --offline` skips fetching entirely and measures against the remote-tracking
refs the last run left under `refs/monosplice/`. It says so once per run on stderr
(`offline: using last-fetched state`), so stdout stays pipeable, and it combines with `--json`
(which gains a top-level `offline: true`; the per-subrepo key set is unchanged) and `--check`.
A subrepo that has never been fetched is reported as `no fetch yet — run without --offline
first` rather than guessed at: with no tracking ref, "never fetched" and "the remote has no
branch" are the same picture from here.

## Exit codes and machine-readable output

Every command exits **0** on success, including "already converged, nothing to do". A refusal —
anything printed as `Error: …` on stderr — exits **2**, and so does a command line clap cannot
parse. Exit **1** is reserved for the three "the work ran, the answer is no" cases: `doctor`
with problems, `status --check` on a subrepo that is not converged, and the collected-failures
report at the end of a multi-subrepo `push`, `pull` or `sync`.

Error messages go to stderr verbatim, newlines and all — no gutter characters, no wrapping — so
they stay readable in a CI log.

`status --check` is the CI form: same human report, but exit 1 unless every subrepo is fully in
sync — nothing to push, nothing to pull, no unreachable remote. `status --json` and
`doctor --json` print one stable object on stdout and nothing else; diagnostics and warnings
always go to stderr, so either can be piped straight into `jq`. The JSON keys are camelCase
(`inSync`, `pullInProgress`, `pushBranch`, `hookError`, `lastExportedMono`) and unchanged from
the TypeScript releases — a pipeline built on them keeps working. Findings are split the same
way everywhere: `problems` is what fails the run, `notes` is informational and never changes an
exit code — including `monorepo.notes`, where settled history (a superseded anchor, an import
trailer from a remote the subrepo no longer tracks) is reported without failing anything.

```sh
monosplice status --check              # 0 = converged, 1 = drift
monosplice status --json | jq '.subrepos[] | select(.inSync | not) | .name'
monosplice doctor --json | jq '.problems'
```

With several subrepos configured, one failing subrepo never silences the others: `push`, `pull`
and `sync` report every failure together at the end and exit 1. The one exception is an import
conflict, which writes the sequencer and therefore stops the run where it stands.

## Install options

monosplice ships as a single static binary. Nothing is compiled at install time and there is no
runtime to keep on your machine — just the binary and the system `git` it shells out to.

The install script detects your platform, downloads the matching tarball from GitHub Releases,
and installs to `/usr/local/bin` (falling back to `~/.local/bin` when it cannot write there):

```sh
curl -fsSL https://github.com/jakequist/monosplice/releases/latest/download/install.sh | sh
```

With cargo, if you would rather build it (the crate is not on crates.io yet):

```sh
cargo install --git https://github.com/jakequist/monosplice
```

From npm, which is now a shim that downloads the same binary on install — for muscle memory,
and for pinning monosplice next to the rest of a JavaScript project's tooling:

```sh
npm install -g monosplice
```

Or by hand, straight from
[GitHub Releases](https://github.com/jakequist/monosplice/releases). Assets are named
`monosplice-<version>-<target>.tar.gz` and contain a single `monosplice` binary, so pinning an
exact artifact is a URL:

```sh
curl -fsSL -o monosplice.tar.gz \
  https://github.com/jakequist/monosplice/releases/download/v1.0.0/monosplice-1.0.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf monosplice.tar.gz && install -m 755 monosplice /usr/local/bin/
```

Released targets:

| Platform | Target triple |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| Linux arm64 | `aarch64-unknown-linux-musl` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple silicon | `aarch64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

### Updating

`monosplice update --check` asks the GitHub Releases API what the newest version is and prints
both, changing nothing:

```console
$ monosplice update --check
installed: 1.0.0
latest:    0.4.1
Run `monosplice update` to install 0.4.1.
```

`monosplice update` does it: it downloads the release asset for the triple this binary was
built for, unpacks it, and renames the new binary over the running one — atomically, so a
half-written binary can never end up on your PATH. Both forms use `curl`, and both fail loudly
rather than guessing: an unreachable API names the releases page, a binary in a directory this
process cannot write names the `curl … | sh` one-liner to reinstall with, and a monosplice
running out of a source checkout (a `target/` directory in its ancestry, or a crate root with a
`.git`) is told to `git pull` instead. Installed by npm or cargo? Update it the way you
installed it, or let `update` swap the binary in place — it replaces whatever executable is
running.

### Shell completion

`monosplice completion <shell>` writes a completion script to stdout; nothing is installed and
nothing on disk is touched.

```sh
monosplice completion bash > /etc/bash_completion.d/monosplice
monosplice completion zsh  > "${fpath[1]}/_monosplice"
monosplice completion fish > ~/.config/fish/completions/monosplice.fish
```

## Releasing

Releases are cut by pushing a tag; nothing is published by hand.

```sh
# 1. bump "version" in Cargo.toml *and* npm/package.json to X.Y.Z
git commit -am "release: vX.Y.Z"
git tag vX.Y.Z && git push origin main vX.Y.Z
```

`.github/workflows/release.yml` then refuses the tag if it disagrees with `Cargo.toml`, runs
the full suite, and cross-builds the five release targets. Each build is packed as
`monosplice-X.Y.Z-<target>.tar.gz` containing one `monosplice` binary; all five, plus
`install.sh`, are attached to the GitHub release, which is what the install one-liner and
`monosplice update` both read. The npm package in `npm/` — the shim that downloads those same
assets — is published last, via [trusted publishing](https://docs.npmjs.com/trusted-publishers):
OIDC, no token secret anywhere, with `release.yml` registered as the trusted publisher in the
package settings on npmjs.com. `.github/workflows/ci.yml` builds and runs the full test suite
on every push to `main` and every pull request.

[`e2e-scenarios.md`](e2e-scenarios.md) is the living backlog. Every scenario has a stable ID
(`S10`, `S42`, …) that its test name references, and items are checked off as their tests land.
New behaviour starts as a new scenario there.
