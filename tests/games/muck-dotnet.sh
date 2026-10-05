#!/bin/sh
# Muck on upstream .NET Mono (docs/unity-dotnet.md, P4): the game's Mono
# library is served by the wasm64 Mono provider.
#
#   tests/games/muck-dotnet.sh OUT RUNDIR [SECS] [gexc flags...]
#
# OUT: tools/unity-dotnet/mono/build.sh's output (provider/mono.wasm,
# lib/System.Private.CoreLib.dll). The BCL directory (CoreLib plus .NET 10's
# managed shared framework) is made in RUNDIR. Env: MUCK (the Linux install,
# default ~/gex-sandbox/games-linux/Muck), GEXC, CORES (optional taskset
# list), DOTNET_FW (the .NET 10 shared framework directory).
#
# Default: batch mode, no graphics. GFX=GLDIR: with graphics, GL through
# gex-gl (GLDIR holds guest/gl/build.sh's x86-64 build) and X from an
# Xwayland run inside the guest on gex's compositor (xwayland-inguest.sh;
# no Xvfb); frames go to RUNDIR/f-*.ppm every EVERY (30) frames.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath "$1"); RUN=$(realpath -m "$2"); SECS=${3:-1800}
shift 2; [ $# -gt 0 ] && shift
MUCK=${MUCK:-$HOME/gex-sandbox/games-linux/Muck}
GEXC=${GEXC:-$HERE/../../target/x86/release/gexc}
FW=${DOTNET_FW:-$(dirname "$(dotnet --list-runtimes | awk '/Microsoft.NETCore.App 10\./ {v=$2; p=$3} END {gsub(/[\[\]]/, "", p); print p "/" v "/x"}')")}
mkdir -p "$RUN/bcl" "$RUN/home"
cp "$FW"/*.dll "$RUN/bcl/"
cp "$OUT/lib/System.Private.CoreLib.dll" "$RUN/bcl/"
cd "$MUCK"
export GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm"
if [ -z "${GFX:-}" ]; then
	timeout -s KILL "$SECS" ${CORES:+taskset -c "$CORES"} \
		"$GEXC" run --mem64 "$@" --env HOME="$RUN/home" --env GEXMONO_ASSEMBLIES="$RUN/bcl" \
		./Muck.x86_64 -batchmode -nographics -logFile "$RUN/player.log" > "$RUN/out.txt" 2>&1 || echo "exit $?"
else
	N=${XNUM:-71}
	[ -e /tmp/.X$N-lock ] && { echo "display :$N is taken (XNUM)"; exit 1; }
	GEX_FPS=1 GEX_PPM_EVERY=${EVERY:-30} timeout -s KILL "$SECS" ${CORES:+taskset -c "$CORES"} \
		"$GEXC" run --mem64 "$@" --gl --wayland "${XDG_RUNTIME_DIR:-/tmp}/muckwl-$$" --wayland-size 800x600 --wayland-out "$RUN/f" \
		--env LD_LIBRARY_PATH="$(realpath "$GFX")" --env HOME="$RUN/home" --env GEXMONO_ASSEMBLIES="$RUN/bcl" \
		/bin/sh "$HERE/xwayland-inguest.sh" $N "$RUN/xwayland.log" \
		./Muck.x86_64 -logFile "$RUN/player.log" -screen-width 800 -screen-height 600 -screen-fullscreen 0 \
		> "$RUN/out.txt" 2>&1 || echo "exit $?"
	# Xwayland's lock and socket node are host files; remove only this display's.
	rm -f /tmp/.X$N-lock /tmp/.X11-unix/X$N
fi
tail -5 "$RUN/player.log"
