#!/bin/sh
# /dev/dsp in the page: what a program plays reaches the global kernel's
# audio ring (shims/kernel/src/audio.rs), which the page's AudioWorklet
# plays (web/src/audio.js). tests/audio/dsp-tone.c plays 441 Hz as
# S16 stereo at 44.1 kHz, closes, waits, then opens again and plays 882 Hz,
# switching to F32 mono at 48 kHz after a first write. Read back from the
# ring at exit (run-node's GEX_AUDIO_TAP): the second stream took the device
# (rate 48000, four generations: two owners, two rates) and its newest
# quarter second is the 882 Hz tone at half amplitude (RMS 0.354).
#
#   tests/browser-audio.sh
#
# Needs gcc and the page's x86-64 shared build (`GEX_ARCH=x86 GEX_MEM64=1
# shims/browser/build.sh`, or without GEX_MEM64; SEED= picks one). A missing piece is a
# skip, not a pass. The single-thread (SP) flavour has no ring yet.
set -u
cd "$(dirname "$0")/.."
SRC=tests/audio/dsp-tone.c
SEED=${SEED:-}
if [ -z "$SEED" ]; then
    SEED=web/public/gex/seed.x86.m64.shared.wasm
    [ -f "$SEED" ] || SEED=web/public/gex/seed.x86.shared.wasm
fi
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
if [ ! -f "$SEED" ] || ! command -v gcc >/dev/null || ! command -v node >/dev/null; then
    echo "skip dsp-tone (page): no seed, gcc or node"; exit 0
fi
jspi=
node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null || jspi=--experimental-wasm-jspi
bin="$WORK/dsp-tone"
gcc -O1 -static -o "$bin" "$SRC" -lm || exit 1
case $SEED in *m64*) m64=1 ;; *) m64= ;; esac
env GEX_ARCH=x86 GEX_MEM64=$m64 GEX_SHARED=1 GEX_AUDIO_TAP=1 timeout 120 node $jspi shims/browser/js/run-node.mjs "$SEED" "$bin" \
    </dev/null >"$WORK/out" 2>"$WORK/err"
st=$?
tap=$(grep '^\[audio\]' "$WORK/err")
if [ $st = 0 ] && grep -qx 'dsp-tone ok' "$WORK/out" && echo "$tap" | awk '
    { for (i = 2; i < NF; i += 2) v[$i] = $(i + 1) }
    END { exit !(v["written"] > 0 && v["rate"] == 48000 && v["gen"] >= 4 &&
                 v["rms"] > 0.32 && v["rms"] < 0.39 && v["hz"] > 850 && v["hz"] < 915) }'; then
    echo "ok   dsp-tone (page): $tap"
else
    echo "FAIL dsp-tone (page): exit $st; ${tap:-no tap}"
    head -5 "$WORK/out" "$WORK/err"
    exit 1
fi
