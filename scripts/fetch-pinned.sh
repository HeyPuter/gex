#!/bin/bash
# A pinned source tree from a persistent bare mirror, never a fresh clone.
# Called by: make sysroot, make sysroot64, make gexcc, make wine (the
# top-level Makefile).
#
#   scripts/fetch-pinned.sh [--worktree] REPO COMMIT MIRROR OUT
#
# OUT gets COMMIT's tree. Without --worktree it is a plain tree (git archive,
# no .git) with OUT/.pinned naming the commit; with --worktree it is a git
# worktree of the mirror, detached at COMMIT, for builds that read git (the
# 64-bit Wine dist keys its steps by the tree's commits). Nothing happens when
# OUT is already at COMMIT. The mirror is a bare repository, made on first
# use, and fetched from only when it lacks COMMIT: by the commit itself (shallow: its tree, not its history), then,
# if the server will not serve a bare commit, every branch. GEX_SRC_SEED=DIR,
# a local clone of the same repository, is fetched from first, so a machine
# with a worktree of the fork needs no network.
set -eu
wt=0; [ "${1:-}" = --worktree ] && { wt=1; shift; }
repo=${1?REPO} commit=${2:?COMMIT} mirror=${3:?MIRROR} out=${4:?OUT}
log() { echo "fetch-pinned: $*" >&2; }
if [ $wt = 1 ]; then
  [ -e "$out/.git" ] && [ "$(git -C "$out" rev-parse HEAD 2>/dev/null)" = "$commit" ] && exit 0
else
  [ "$(cat "$out/.pinned" 2>/dev/null)" = "$commit" ] && exit 0
  # a tree an older version made, marked .wine32-src: kept, and marked .pinned
  [ "$(cat "$out/.wine32-src" 2>/dev/null)" = "$commit" ] && { echo "$commit" > "$out/.pinned"; exit 0; }
fi
have() { git --git-dir="$mirror" cat-file -e "$commit^{commit}" 2>/dev/null; }
[ -d "$mirror" ] || { mkdir -p "$(dirname "$mirror")"; git init -q --bare "$mirror"; }
if ! have && [ -n "${GEX_SRC_SEED:-}" ]; then
  log "fetching $commit from $GEX_SRC_SEED"
  git --git-dir="$mirror" fetch -q "$GEX_SRC_SEED" "$commit" 2>/dev/null ||
    git --git-dir="$mirror" fetch -q "$GEX_SRC_SEED" '+refs/heads/*:refs/seed/*' '+refs/remotes/*:refs/seed-remotes/*' || true
fi
if ! have; then
  [ -n "$repo" ] || { log "$commit is not in $mirror and no repository is pinned for it"; exit 1; }
  log "fetching $commit from $repo"
  # the commit alone (shallow), else every branch
  git --git-dir="$mirror" fetch -q --depth 1 "$repo" "$commit" 2>/dev/null ||
    git --git-dir="$mirror" fetch -q "$repo" '+refs/heads/*:refs/heads/*'
fi
have || { log "$commit is not in $repo"; exit 1; }
# Keep the commit reachable in the mirror (a later gc must not drop it).
git --git-dir="$mirror" update-ref "refs/pins/$commit" "$commit"
if [ $wt = 1 ]; then
  if [ -e "$out/.git" ]; then
    [ -z "$(git -C "$out" status --porcelain --untracked-files=no)" ] || { log "$out has uncommitted changes; not moving it to $commit"; exit 1; }
    git -C "$out" checkout -q --detach "$commit"
  else
    git --git-dir="$mirror" worktree add -q --detach "$out" "$commit"
  fi
else
  rm -rf "$out.tmp" && mkdir -p "$out.tmp"
  git --git-dir="$mirror" archive "$commit" | tar -x -C "$out.tmp"
  echo "$commit" > "$out.tmp/.pinned"
  rm -rf "$out" && mv "$out.tmp" "$out"
fi
log "$out"
