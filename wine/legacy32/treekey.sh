#!/bin/bash
# The tree hash of a git worktree as it is on disk: committed, staged and
# unstaged changes and untracked (not ignored) files, through a scratch copy
# of its index, so the worktree's own index is never touched. Only files
# whose stat changed since the index was written are hashed again, so a
# second call on an unchanged tree takes well under a second.
#
#   wine32/treekey.sh DIR      prints the tree hash (40 hex)
set -eu
dir=$1
gitdir=$(git -C "$dir" rev-parse --git-dir)
case $gitdir in /*) ;; *) gitdir=$dir/$gitdir ;; esac
idx=$(mktemp "${TMPDIR:-/tmp}/wine32-treekey.XXXXXX")
trap 'rm -f "$idx"' EXIT
cp "$gitdir/index" "$idx" 2>/dev/null || true
GIT_INDEX_FILE=$idx git -C "$dir" add -A . >/dev/null 2>&1
GIT_INDEX_FILE=$idx git -C "$dir" write-tree
