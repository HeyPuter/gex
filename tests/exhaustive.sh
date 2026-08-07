#!/bin/sh
# Exhaustively translate every executable in a root filesystem and check that
# what comes out is valid wasm.
#
#   tests/exhaustive.sh <rootfs> [max-bytes]
#
# This is the widest lifter test there is, and it is cheap. Exhaustive
# translation starts a block at every decodable address in every executable
# segment, so it reaches encodings that no program gex has ever *run* would
# execute -- the far side of a branch nothing takes, an instruction a library
# only uses on a CPU with a feature bit gex does not set, a literal pool that
# happens to decode. A differential test can only cover what someone thought
# to write; this covers what Debian shipped.
#
# It cannot check answers -- nothing is executed -- so what it finds is a
# narrower class: modules that are not well-formed. That class is worth
# finding on its own, because a module that does not validate does not run at
# all, and because the lifter producing one always means it got a type wrong,
# which usually means it got the instruction wrong too. `smov x0, v0.h[2]`
# was found exactly this way, 182 MB into an exhaustive CPython: the lane
# extract yields 32 bits and the lifter asked for 64.
#
# Blocks ending in an instruction gex cannot execute are counted, not failed.
# Exhaustive translation decodes padding and data, so a large count is normal
# and only its *growth* is interesting.
set -eu

R="${1:?usage: tests/exhaustive.sh <rootfs> [max-bytes]}"
MAX="${2:-8000000}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
GEXC="$HERE/target/release/gexc"
WORK="${TMPDIR:-/tmp}/gex-exhaustive"

[ -x "$GEXC" ] || { echo "skip: build gexc first"; exit 0; }
mkdir -p "$WORK"

ok=0
bad=0
skipped=0
illegal_total=0

# Every ELF in the usual places, plus the shared libraries, largest last so a
# failure on something small is reported before the slow ones run.
find "$R/usr/bin" "$R/usr/sbin" "$R/bin" "$R/sbin" "$R/usr/lib" "$R/lib" \
     -type f -printf '%s\t%p\n' 2>/dev/null | sort -n | cut -f2 \
     > "$WORK/candidates"

while read -r f; do
    # ELF only, and only aarch64 ones.
    head -c 20 "$f" 2>/dev/null | od -An -tx1 | tr -d ' \n' | \
        grep -q "^7f454c4602010100.*b700$" || continue
    size=$(stat -c%s "$f")
    if [ "$size" -gt "$MAX" ]; then
        skipped=$((skipped + 1))
        continue
    fi
    out="$WORK/out.wasm"
    if report=$("$GEXC" build --exhaustive "$f" -o "$out" 2>&1); then
        ok=$((ok + 1))
        n=$(printf '%s' "$report" | sed -n 's/.*: \([0-9]*\) blocks end in an instruction.*/\1/p')
        illegal_total=$((illegal_total + ${n:-0}))
    else
        bad=$((bad + 1))
        echo "FAIL $f"
        printf '%s\n' "$report" | sed 's/^/     /' | head -4
    fi
    rm -f "$out"
done < "$WORK/candidates"

echo "$ok validated, $bad failed, $skipped skipped for size (> $MAX bytes)"
echo "$illegal_total blocks ended in an instruction gex cannot execute"
[ "$bad" -eq 0 ]
