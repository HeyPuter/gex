#!/bin/sh
# A wasm32 busybox as the guest's /bin/sh and coreutils (`gexc run
# --busybox`): a script with pipelines, command substitution, redirections,
# externals and a failing command, against the same script under the
# host's sh. BUSYBOX names the binary (tools/wine-wasm/build-busybox.sh);
# without it this is a skip.
#
#   tests/wasm32/busybox.sh GEXC
set -u
GEXC=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
[ -n "${BUSYBOX:-}" ] && [ -f "$BUSYBOX" ] || { echo "skip busybox (set BUSYBOX)"; exit 0; }
WORK=$(mktemp -d)
cd "$WORK"
printf 'b\na\nc\nb\n' > in.txt
script='echo b a | tr " " "\n" | sort | head -1
cat in.txt | sort | uniq | wc -l
x=$(grep -c b in.txt); echo "b lines: $x"
sed s/b/B/ in.txt | grep -c B
mkdir -p d/e && cp in.txt d/e/ && ls d/e && rm -rf d
for i in 1 2 3; do echo $i; done | tail -1
awk "{n++} END {print n}" in.txt
false || echo "false failed"
/nonexistent-command 2>/dev/null; echo "status $?"
echo done'
native=$(sh -c "$script" 2>&1)
gex=$("$GEXC" run --busybox "$BUSYBOX" /bin/sh -c "$script" 2>&1)
if [ "$native" = "$gex" ]; then
    echo "ok   busybox"
else
    echo "FAIL busybox"
    printf '%s\n' "$native" > native.out; printf '%s\n' "$gex" > gex.out; diff native.out gex.out | head -20
    exit 1
fi
