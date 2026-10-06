#!/bin/sh
# gexc run --bind HOST=GUEST: a host file and a host directory mounted at
# guest paths (over a file that exists and at a path that does not).
#
#   tests/games/bind.sh [GEXC]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
gexc=${1:-$here/../../target/x86/release/gexc}
work=${TMPDIR:-/tmp}/gex-bind-$(id -u)
rm -rf "$work"; mkdir -p "$work/dir"
echo "bound file" > "$work/file"
echo "in the bound dir" > "$work/dir/inside"
fail=0
out=$("$gexc" run --no-cgen --bind "$work/file=/etc/hostname" /bin/cat /etc/hostname 2>/dev/null)
[ "$out" = "bound file" ] && echo "ok   a file over an existing guest file" || { echo "FAIL file: $out"; fail=1; }
out=$("$gexc" run --no-cgen --bind "$work/dir=/nonexistent/gex-bind" /bin/cat /nonexistent/gex-bind/inside 2>/dev/null)
[ "$out" = "in the bound dir" ] && echo "ok   a directory at a path that does not exist" || { echo "FAIL dir: $out"; fail=1; }
"$gexc" run --no-cgen --bind "$work/dir=/nonexistent/gex-bind" /bin/sh -c 'echo new > /nonexistent/gex-bind/written' 2>/dev/null
[ "$(cat "$work/dir/written" 2>/dev/null)" = "new" ] && echo "ok   writes land in the host directory" || { echo "FAIL write"; fail=1; }
rm -rf "$work"
exit $fail
