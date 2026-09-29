#!/bin/sh
# Regenerates the bundles in this directory: a chain of monorepos synced by monosplice 1.0.0,
# which copied the sync trailers of the commit it replayed into the commit it wrote. What it
# leaves behind is already published and can never be rewritten, so tests/e2e_chained.rs
# restores it and checks that the current binary copes with it as it is.
#
#   MS=/path/to/monosplice-1.0.0 sh tests/fixtures/v1.0.0-chain/generate.sh
#
#   leaf.git    spliced out of middle's lib/; its tip carries TWO Monosplice-Source trailers,
#               the outer monorepo's sha first (bug 2)
#   middle.git  the middle monorepo (mw below, pushed)
#   mw.bundle   middle's working clone; its config publishes lib/ to ../leaf.git
#   outer       vendors middle at vendor/middle (snapshot attach); one import carries
#               Monosplice-Origin: <leaf sha> + Monosplice-Origin: <middle sha> (bug 3)
#   outer5      vendors middle with --import-history; the replay of middle's leaf import
#               carries the same doubled Origin (bug 5)
#
# Every remote is a relative path, so the bundles restore anywhere side by side.
set -eu
MS=${MS:?set MS to a monosplice 1.0.0 binary}
"$MS" --version | grep -q '^monosplice 1\.0\.0$' || { echo "MS must be monosplice 1.0.0" >&2; exit 1; }

OUT=$(cd "$(dirname "$0")" && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
cd "$T"

export HOME="$T/home" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$T/home/.gitconfig"
export GIT_AUTHOR_NAME=dev GIT_AUTHOR_EMAIL=dev@example.test
export GIT_COMMITTER_NAME=dev GIT_COMMITTER_EMAIL=dev@example.test
mkdir "$HOME"
git config --global init.defaultBranch main
git config --global commit.gpgsign false
git config --global pull.rebase false

N=0
tick() {
  N=$((N + 1))
  export GIT_AUTHOR_DATE="$((1780000000 + N * 61)) +0000" GIT_COMMITTER_DATE="$((1780000000 + N * 61)) +0000"
}

git init -q --bare leaf.git
git init -q --bare middle.git

git init -q mw
cd mw
mkdir lib app
echo "lib v1" > lib/a.txt
echo app > app/main.txt
printf '[[subrepos]]\npath = "lib"\nremote = "../leaf.git"\n' > monosplice.toml
git add .; tick; git commit -qm "middle: initial"
tick; "$MS" attach lib --yes >/dev/null
git remote add origin ../middle.git
git push -q origin main
cd "$T"

for o in outer outer5; do
  git init -q "$o"
  cd "$o"
  echo "$o" > README
  printf '[[subrepos]]\nname = "middle"\npath = "vendor/middle"\nremote = "../middle.git"\n' > monosplice.toml
  git add .; tick; git commit -qm "$o: initial"
  cd "$T"
done
cd outer; tick; "$MS" attach vendor/middle >/dev/null; cd "$T"

# Bug 3: leaf -> middle -> outer. The outer import copies middle's Origin and appends its own.
git clone -q leaf.git lw
cd lw
echo "from leaf" > b.txt
git add .; tick; git commit -qm "leaf: add b"
git push -q origin main
cd "$T/mw"; tick; "$MS" pull lib >/dev/null; git push -q origin main
cd "$T/outer"; tick; "$MS" pull middle >/dev/null
cd "$T"

# Bug 5: the same doubled Origin, produced by an --import-history replay.
cd outer5; tick; "$MS" attach vendor/middle --import-history >/dev/null; cd "$T"

# Bug 2: outer -> middle -> leaf. Middle's export copies outer's Source and appends its own.
cd outer
echo "outer patch" >> vendor/middle/lib/a.txt
tick; git commit -qam "outer: patch lib"
tick; "$MS" push middle >/dev/null
cd "$T/mw"; git pull -q origin main; tick; "$MS" push lib >/dev/null
cd "$T"

sources=$(git -C leaf.git log -1 --format='%(trailers:key=Monosplice-Source,valueonly)' main | grep -c .)
[ "$sources" -eq 2 ] || { echo "expected a doubled Source trailer on leaf, got $sources" >&2; exit 1; }

git -C leaf.git bundle create "$OUT/leaf.bundle" main 2>/dev/null
git -C middle.git bundle create "$OUT/middle.bundle" main 2>/dev/null
git -C mw bundle create "$OUT/mw.bundle" main 2>/dev/null
git -C outer bundle create "$OUT/outer.bundle" main 2>/dev/null
git -C outer5 bundle create "$OUT/outer5.bundle" main 2>/dev/null
echo "wrote $OUT/{leaf,middle,mw,outer,outer5}.bundle"
