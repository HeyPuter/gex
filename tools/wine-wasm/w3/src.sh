#!/bin/bash
# A pinned source tree from a persistent bare mirror, never a fresh clone.
#
#   w3/src.sh REPO COMMIT MIRROR OUT
#
# OUT gets COMMIT's tree (git archive, no .git) and OUT/.w3-src naming the
# commit. Nothing happens when OUT/.w3-src already names COMMIT. The mirror
# (a bare repository, made on first use) is fetched from only when it lacks
# COMMIT: by the commit itself, then, if the server will not serve a bare
# commit, every branch. W3_SEED=DIR (a local clone of the same repository)
# is fetched from first, so a machine with a worktree of the fork needs no
# network at all.
set -eu
repo=$1 commit=$2 mirror=$3 out=$4
[ "$(cat "$out/.w3-src" 2>/dev/null)" = "$commit" ] && exit 0
have() { git --git-dir="$mirror" cat-file -e "$commit^{commit}" 2>/dev/null; }
if [ ! -d "$mirror" ]; then
  mkdir -p "$(dirname "$mirror")"
  git init -q --bare "$mirror"
fi
if ! have && [ -n "${W3_SEED:-}" ]; then
  echo "w3/src: fetching $commit from $W3_SEED" >&2
  git --git-dir="$mirror" fetch -q "$W3_SEED" "$commit" 2>/dev/null ||
    git --git-dir="$mirror" fetch -q "$W3_SEED" '+refs/heads/*:refs/seed/*' '+refs/remotes/*:refs/seed-remotes/*' || true
fi
if ! have; then
  echo "w3/src: fetching $commit from $repo" >&2
  git --git-dir="$mirror" fetch -q "$repo" "$commit" 2>/dev/null ||
    git --git-dir="$mirror" fetch -q "$repo" '+refs/heads/*:refs/heads/*'
fi
have || { echo "w3/src: $commit is not in $repo" >&2; exit 1; }
# Keep the commit reachable in the mirror (a later gc must not drop it).
git --git-dir="$mirror" update-ref "refs/pins/$commit" "$commit"
rm -rf "$out.tmp" && mkdir -p "$out.tmp"
git --git-dir="$mirror" archive "$commit" | tar -x -C "$out.tmp"
echo "$commit" > "$out.tmp/.w3-src"
rm -rf "$out" && mv "$out.tmp" "$out"
echo "w3/src: $out" >&2
