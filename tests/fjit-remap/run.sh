#!/bin/sh
# Code unmapped and replaced at the same address (remap.c): output under gexc
# --fjit must match native.
#
#   tests/fjit-remap/run.sh [gexc]      (needs zig for the musl target)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
W=${WORK:-$(mktemp -d)}
mkdir -p "$W"
zig cc -target x86_64-linux-musl -O1 -static -o "$W/remap" "$HERE/remap.c" || exit 1
native=$("$W/remap")
fail=0
for flags in "" "--fjit"; do
	gex=$("$GEXC" run $flags "$W/remap" 2>&1)
	if [ "$native" = "$gex" ]; then
		echo "ok   remap ${flags:-t1}"
	else
		echo "FAIL remap ${flags:-t1}"; printf 'native:\n%s\ngex:\n%s\n' "$native" "$gex"; fail=1
	fi
done
exit $fail
