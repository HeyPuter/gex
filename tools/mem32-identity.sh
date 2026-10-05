#!/usr/bin/env bash
# The memory32 path stays byte-identical (docs/mem64.md): a manifest of what
# gex generates for wasm32 guests, to compare before and after a change.
#
#   tools/mem32-identity.sh OUT.manifest
#
# For the Tier 0 corpus (pewasm/bench/programs, as bench/build.py builds it)
# on ga64, gx64 and gi386: the sha256 of `gexc build --exhaustive
# --no-shared-memory` (tier-1, as the bench builds it), of `gexc build --fjit`
# (the function JIT's whole-image form), and on gx64 of `gexc build --cgen`
# (a function-tier unit). For a few PE images: the unit key `cgen-image`
# caches under, its object names (a hash of each unit's C) and its module.
#
#   GEXC_A64   gexc for AArch64 guests (default target/release/gexc)
#   GEXC_X86   gexc for x86 guests (default target/x86/release/gexc)
#   CORPUS     the bench sources (default ../../pewasm/bench beside this repo)
#   WORK       scratch (corpus ELFs are built once and kept here)
#
# Then `diff A.manifest B.manifest` must print nothing.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:?usage: tools/mem32-identity.sh OUT.manifest}
GEXC_A64=$(realpath "${GEXC_A64:-$ROOT/target/release/gexc}")
GEXC_X86=$(realpath "${GEXC_X86:-$ROOT/target/x86/release/gexc}")
CORPUS=${CORPUS:-$(realpath "$ROOT/../../pewasm/bench" 2>/dev/null || realpath "$ROOT/../pewasm/bench")}
WORK=${WORK:-${TMPDIR:-/tmp}/gex-mem32-identity}
mkdir -p "$WORK"
: > "$OUT"
sha() { sha256sum "$1" | cut -c1-64; }

declare -A ZIG=([ga64]=aarch64-linux-musl [gx64]=x86_64-linux-musl [gi386]=x86-linux-musl)
declare -A GEXC=([ga64]=$GEXC_A64 [gx64]=$GEXC_X86 [gi386]=$GEXC_X86)
for t in ga64 gx64 gi386; do
    mkdir -p "$WORK/$t"
    extra=(); [ $t = gi386 ] && extra=(-Wl,--image-base=0x1000000)
    for d in "$CORPUS"/programs/*/; do
        n=$(basename "$d")
        src=$d$n.c
        [ -f "$src" ] || src=$d$n.cc
        [ -f "$src" ] || continue
        cc=cc; [ "${src##*.}" = cc ] && cc=c++
        elf=$WORK/$t/$n
        # The same command as bench/build.py's build_gex.
        [ -x "$elf" ] || zig $cc -target ${ZIG[$t]} -O2 -static -ffp-contract=off "${extra[@]}" \
            -o "$elf" "$src" -lm 2>/dev/null || { echo "$t $n elf-build-failed" >> "$OUT"; continue; }
        g=${GEXC[$t]}
        if GEX_CGEN_CACHE=$WORK/cache "$g" build --exhaustive --no-shared-memory "$elf" -o "$WORK/$t/$n.t1.wasm" >/dev/null 2>&1; then
            echo "$t $n tier1 $(sha "$WORK/$t/$n.t1.wasm")" >> "$OUT"
        else
            echo "$t $n tier1 failed" >> "$OUT"
        fi
        if GEX_CGEN_CACHE=$WORK/cache "$g" build --fjit "$elf" -o "$WORK/$t/$n.fjit.wasm" >/dev/null 2>&1; then
            echo "$t $n fjit $(sha "$WORK/$t/$n.fjit.wasm")" >> "$OUT"
        else
            echo "$t $n fjit failed" >> "$OUT"
        fi
        if [ $t = gx64 ]; then
            if GEX_CGEN_CACHE=$WORK/cache "$g" build --cgen --no-cache "$elf" -o "$WORK/$t/$n.cgen.wasm" >/dev/null 2>&1; then
                echo "$t $n cgen $(sha "$WORK/$t/$n.cgen.wasm")" >> "$OUT"
            else
                echo "$t $n cgen failed" >> "$OUT"
            fi
        fi
    done
done

# Function-tier units of PE images: the key they are cached under, and the
# C. Unit sources are deleted once compiled, but each object is cached as
# <hash of its C, the header, the flags and clang's version>.o, so with a
# fresh cache the object names are a fingerprint of the C output.
for pe in "$ROOT"/tests/nt/bin/x64/sync.exe "$ROOT"/tests/nt/bin/x86/sync.exe \
          "$ROOT"/tests/nt/bin/x64/cxx.exe "$ROOT"/tests/nt/bin/x86/cxx.exe; do
    rel=${pe#"$ROOT"/}
    rm -rf "$WORK/pe-cache"
    log=$(GEX_CGEN_CACHE=$WORK/pe-cache "$GEXC_X86" cgen-image "$pe" 2>&1 || true)
    key=$(echo "$log" | grep -o 'pe-cache/[0-9a-f]\{64\}' | head -1 | cut -d/ -f2)
    echo "pe $rel key ${key:-none}" >> "$OUT"
    ls "$WORK/pe-cache/objects" 2>/dev/null | sort | sed "s|^|pe $rel c |" >> "$OUT"
    [ -n "$key" ] && echo "pe $rel wasm $(sha "$WORK/pe-cache/$key/c.wasm")" >> "$OUT"
done
echo "$(wc -l < "$OUT") lines in $OUT"
