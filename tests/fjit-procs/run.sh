#!/bin/sh
# The function JIT keeps what it learns per guest process: a parent and the
# image its child execs share addresses, not code (procs.c). Output under
# gexc --fjit must match native.
#
#   tests/fjit-procs/run.sh [gexc]      (needs zig for the musl target)
set -u
# Output is compared with the native run: not even gex's default lines (log.mjs).
export GEX_LOG=${GEX_LOG:-off}
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
W=${WORK:-$(mktemp -d)}
mkdir -p "$W"
zig cc -target x86_64-linux-musl -O1 -static -o "$W/procs" "$HERE/procs.c" || exit 1
zig cc -target x86_64-linux-musl -O1 -static -DCHILD -o "$W/procs-child" "$HERE/procs.c" || exit 1
pa=$(nm "$W/procs" | awk '$3=="probe"{print $1}'); ca=$(nm "$W/procs-child" | awk '$3=="__memset_gexprobe"{print $1}')
[ "$pa" = "$ca" ] || { echo "FAIL: probe at $pa in the parent but __memset_gexprobe at $ca in the child (the test needs them equal)"; exit 1; }
native=$("$W/procs" "$W/procs-child")
fail=0
for flags in "" "--fjit"; do
	gex=$("$GEXC" run $flags "$W/procs" "$W/procs-child" 2>&1)
	if [ "$native" = "$gex" ]; then
		echo "ok   procs ${flags:-t1}"
	else
		echo "FAIL procs ${flags:-t1}"; printf 'native:\n%s\ngex:\n%s\n' "$native" "$gex"; fail=1
	fi
done
exit $fail
