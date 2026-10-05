#!/bin/sh
# A wasm32 busybox as the guest's /bin/sh and coreutils (`gexc run
# --busybox`): a script with pipelines, command substitution, subshells,
# redirections, externals and a failing command (hush forks for them),
# against the same script under the host's sh. BUSYBOX names the binary (tools/wine-wasm/build-busybox.sh);
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
(cd / && pwd); x=1; (x=2; echo "sub $x"); echo "top $x"
{ echo a; echo b; } | sort -r | tr "\n" " "; echo
echo $(echo $(echo nested))
f() { echo "in f $1"; }; f one | cat
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
# An x86-64 shell (the host's, translated) running the wasm32 busybox and
# going on afterwards: the child must not switch the translator to i386.
mixed=$("$GEXC" run /bin/sh -c "$BUSYBOX echo hi | $BUSYBOX tr a-z A-Z; echo after" 2>&1)
if [ "$mixed" = "$(printf 'HI\nafter')" ]; then
    echo "ok   busybox under an x86-64 shell"
else
    echo "FAIL busybox under an x86-64 shell"; printf '%s\n' "$mixed" | tail -5
    exit 1
fi
cd / && rm -rf "$WORK"
