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
# tests/audio/dsp-pace.c writes a second of audio as fast as the device
# takes it, under run-node's stand-in player (GEX_AUDIO_PLAYER) at four
# times and at half the real rate: the device must pace its writer to the
# player, not the wall clock (the 186 ms buffer aside, about 0.2 s and
# 1.6 s; by the wall clock both would take about 0.8 s).
#
#   tests/browser-audio.sh
#
# tests/audio/dsp-impulse.c times an impulse's round trip through the device
# and the stand-in player's loopback (GEX_AUDIO_LOOP=1, the page's
# `?audioloop=1`), with the 256-frame buffer it asks for
# (SNDCTL_DSP_SETFRAGMENT): under 20 ms, where the old fixed 32 KiB buffer
# made it about 170 ms. tests/audio/dsp-mix.c plays two streams at once and
# reads the loopback: mixed, RMS 0.25 (one stream alone would be 0.177).
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
pace="$WORK/dsp-pace"
gcc -O1 -static -o "$pace" tests/audio/dsp-pace.c || exit 1
for speed in 4 0.5; do
    env GEX_ARCH=x86 GEX_MEM64=$m64 GEX_SHARED=1 GEX_AUDIO_PLAYER=$speed timeout 120 node $jspi shims/browser/js/run-node.mjs "$SEED" "$pace" \
        </dev/null >"$WORK/pace-$speed" 2>"$WORK/pace-$speed.err"
    st=$?
    ms=$(sed -n 's/^dsp-pace ms //p' "$WORK/pace-$speed")
    case $speed in 4) lo=0; hi=600 ;; *) lo=1200; hi=100000 ;; esac
    if [ $st = 0 ] && [ -n "$ms" ] && [ "$ms" -ge $lo ] && [ "$ms" -lt $hi ]; then
        echo "ok   dsp-pace (page, player at ${speed}x): a second of audio taken in $ms ms"
    else
        echo "FAIL dsp-pace (page, player at ${speed}x): exit $st, ${ms:-no} ms (want $lo-$hi)"
        head -5 "$WORK/pace-$speed" "$WORK/pace-$speed.err"
        exit 1
    fi
done
imp="$WORK/dsp-impulse"
gcc -O1 -static -o "$imp" tests/audio/dsp-impulse.c || exit 1
env GEX_ARCH=x86 GEX_MEM64=$m64 GEX_SHARED=1 GEX_AUDIO_PLAYER=1 GEX_AUDIO_LOOP=1 timeout 120 node $jspi shims/browser/js/run-node.mjs "$SEED" "$imp" 9 2 \
    </dev/null >"$WORK/imp" 2>"$WORK/imp.err"
st=$?
med=$(sed -n 's/^dsp-impulse rt ms [^ ]* \([^ ]*\) .*/\1/p' "$WORK/imp")
if [ $st = 0 ] && [ -n "$med" ] && awk -v m="$med" 'BEGIN { exit !(m > 0 && m < 20) }'; then
    echo "ok   dsp-impulse (page, 256-frame buffer): $(sed -n 's/^dsp-impulse //p' "$WORK/imp" | tr '\n' ';')"
else
    echo "FAIL dsp-impulse (page): exit $st, median ${med:-none} ms (want under 20)"
    head -5 "$WORK/imp" "$WORK/imp.err"
    exit 1
fi
mix="$WORK/dsp-mix"
gcc -O1 -static -o "$mix" tests/audio/dsp-mix.c -lm || exit 1
env GEX_ARCH=x86 GEX_MEM64=$m64 GEX_SHARED=1 GEX_AUDIO_PLAYER=1 GEX_AUDIO_LOOP=1 timeout 120 node $jspi shims/browser/js/run-node.mjs "$SEED" "$mix" \
    </dev/null >"$WORK/mix" 2>"$WORK/mix.err"
st=$?
rms=$(sed -n 's/^dsp-mix rms \([^ ]*\).*/\1/p' "$WORK/mix")
if [ $st = 0 ] && [ -n "$rms" ] && awk -v r="$rms" 'BEGIN { exit !(r > 0.23 && r < 0.27) }'; then
    echo "ok   dsp-mix (page): two streams mixed, loopback RMS $rms"
else
    echo "FAIL dsp-mix (page): exit $st, RMS ${rms:-none} (want 0.25)"
    head -5 "$WORK/mix" "$WORK/mix.err"
    exit 1
fi
