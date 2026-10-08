#!/bin/bash
# The Unity game legs of a gate, run on the bench box (games never run on gooner):
#
#   tests/games/unity-edger-gate.sh TREE [OUT]
#
# TREE: the gate's worktree, with its gexc built (TREE/target/coord-x86/release/gexc, or GEXC=).
# OUT: where results land on gooner (default ~/gex-sandbox/scratch/unity/edger-gate/<TREE's head>).
# Legs, each PASS/FAIL:
#   gex-gl    the gex-gl host tests built from TREE, on edger's NVIDIA GL (the framebuffer-name test
#             only fails there);
#   muck      Muck on the .NET provider (DNC, default ~/gex-sandbox/scratch/unity/dnc) reaches
#             "Initializing packets" and presents a non-black frame after it;
#   hk        Hollow Knight reaches its menu (the unload after Menu_Title) and presents a
#             non-black frame after it.
# The game legs run with the .NET wasm JIT on (the default; JIT=0 for the interpreter only).
# Each game leg keeps its summary and its last frame as PNG in OUT. Runs go through
# scratch/bench/bench.sh (BENCH_HOST, edger by default), one at a time under its lock, with
# tests/games/unity-bench.sh. Takes about 25 min with a warm GEX_FJIT_CACHE, 40 cold.
# Needs edger's staging (docs/unity-dotnet.md §12 item 21): the games, gl64, Xwayland and
# LLVM 22 under scratch/unity/edger, .NET 10.0.10's shared framework.
set -u
TREE=$(realpath "$1")
HEAD=$(git -C "$TREE" rev-parse --short HEAD)
OUT=${2:-$HOME/gex-sandbox/scratch/unity/edger-gate/$HEAD}
GEXC=${GEXC:-$TREE/target/coord-x86/release/gexc}
DNC=${DNC:-$HOME/gex-sandbox/scratch/unity/dnc}
B=$HOME/gex-sandbox/scratch/bench/bench.sh
ST=$HOME/gex-sandbox/scratch/unity/edger-gate/stage-$HEAD
mkdir -p "$OUT" "$ST"
[ -x "$GEXC" ] || { echo "no gexc at $GEXC"; exit 2; }
cp "$GEXC" "$ST/gexc"
# The gex-gl host tests from the gate's tree (a target directory of their own).
( cd "$TREE" && CARGO_BUILD_JOBS=${JOBS:-6} nice -n 10 cargo test --release -p gex-gl --no-run \
	--target-dir "$TREE/target/edger-gltest" 2> "$OUT/gex-gl-build.log" ) || { echo "gex-gl: FAIL (build)"; exit 1; }
T=$(ls -t "$TREE"/target/edger-gltest/release/deps/gex_gl-* | grep -v '\.d$' | head -1)
cp "$T" "$ST/gex-gl-tests"
# The provider the gate built: only what a run reads.
mkdir -p "$ST/dnc/provider"
cp "$DNC/provider/mono.wasm" "$ST/dnc/provider/"
cp -r "$DNC/lib" "$ST/dnc/"
[ -d "$DNC/lib-extra" ] && cp -r "$DNC/lib-extra" "$ST/dnc/"
rc=0
# 1. gex-gl
"$B" -s "$ST" -s "$TREE/tests/games" -- "$ST/gex-gl-tests" --test-threads 1 > "$OUT/gex-gl.txt" 2>&1
if grep -q "^test result: ok" "$OUT/gex-gl.txt"; then echo "gex-gl: PASS ($(grep -o '[0-9]* passed' "$OUT/gex-gl.txt"))"
else echo "gex-gl: FAIL"; grep -E "FAILED|panicked" "$OUT/gex-gl.txt" | head -5; rc=1; fi
# 2. and 3. the games
leg() { # NAME GAME SECS MARKER
	local name=$1 game=$2 secs=$3 marker=$4 run=$HOME/gex-sandbox/scratch/unity/edger-gate/run-$HEAD-$1
	"$B" -- env GEXC="$ST/gexc" DNC="$ST/dnc" GEX_FJIT_CACHE=1 EVERY=600 \
		bash "$TREE/tests/games/unity-bench.sh" "$game" "$run" "$secs" --fjit --env GEXMONO_WASM_JIT="${JIT:-1}" > "$OUT/$name.txt" 2>&1
	mkdir -p "$OUT/$name-frames"
	rsync -a --include='f-*.ppm' --include=plog-t.txt --exclude='*' "${BENCH_HOST_SSH:-claude@edger}:$run/" "$OUT/$name-frames/"
	python3 - "$OUT/$name-frames" "$OUT/$name.png" "$marker" "$OUT/$name.txt" <<-'EOF'
	import glob, re, sys
	from PIL import Image, ImageStat
	d, png, marker, summary = sys.argv[1:5]
	text = open(summary, errors="replace").read()
	m = re.search(re.escape(marker) + r": ([\d.]+) s", text)
	frames = sorted(glob.glob(d + "/f-*.ppm"), key=lambda f: int(re.search(r"f-(\d+)", f).group(1)))
	bright = 0.0
	if frames:
	    im = Image.open(frames[-1]); im.save(png)
	    bright = sum(ImageStat.Stat(im).mean) / 3
	fps = re.search(r"fps window [\d-]+ s: ([\d.]+) presents/s", text)
	ok = m is not None and bright > 5 and fps is not None and float(fps.group(1)) > 0
	print(f"{'PASS' if ok else 'FAIL'}: {marker} at {m.group(1) if m else 'never'} s, "
	      f"menu {fps.group(1) if fps else '-'} presents/s, last frame brightness {bright:.1f} ({png})")
	sys.exit(0 if ok else 1)
	EOF
}
r=$(leg muck muck 600 "Muck packets"); echo "muck: $r"; case $r in PASS*) ;; *) rc=1;; esac
r=$(leg hk hk 660 "HK menu ready (post-unload)"); echo "hk: $r"; case $r in PASS*) ;; *) rc=1;; esac
exit $rc
