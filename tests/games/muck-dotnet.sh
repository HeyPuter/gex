#!/bin/sh
# Muck on upstream .NET Mono (docs/unity-dotnet.md, P4): the game's Mono
# library is served by the wasm64 Mono provider; batch mode, no graphics.
#
#   tests/games/muck-dotnet.sh OUT RUNDIR [SECS] [gexc flags...]
#
# OUT: tools/unity-dotnet/mono/build.sh's output (provider/mono.wasm,
# lib/System.Private.CoreLib.dll). The BCL directory (CoreLib plus .NET 10's
# managed shared framework) is made in RUNDIR. Env: MUCK (the install,
# default ~/.local/share/Steam/steamapps/common/Muck), GEXC, CORES (1,7),
# DOTNET_FW (the .NET 10 shared framework directory).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath "$1"); RUN=$(realpath -m "$2"); SECS=${3:-1800}
shift 2; [ $# -gt 0 ] && shift
MUCK=${MUCK:-$HOME/.local/share/Steam/steamapps/common/Muck}
GEXC=${GEXC:-$HERE/../../target/x86/release/gexc}
FW=${DOTNET_FW:-$(dirname "$(dotnet --list-runtimes | awk '/Microsoft.NETCore.App 10\./ {v=$2; p=$3} END {gsub(/[\[\]]/, "", p); print p "/" v "/x"}')")}
mkdir -p "$RUN/bcl" "$RUN/home"
cp "$FW"/*.dll "$RUN/bcl/"
cp "$OUT/lib/System.Private.CoreLib.dll" "$RUN/bcl/"
cd "$MUCK"
GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm" timeout -s KILL "$SECS" taskset -c "${CORES:-1,7}" \
	"$GEXC" run --mem64 "$@" --env HOME="$RUN/home" --env GEXMONO_ASSEMBLIES="$RUN/bcl" \
	./Muck.x86_64 -batchmode -nographics -logFile "$RUN/player.log" > "$RUN/out.txt" 2>&1 || echo "exit $?"
tail -5 "$RUN/player.log"
