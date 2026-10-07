#!/bin/bash
# A Unity game on the .NET Mono provider with graphics, measured (docs/unity-dotnet.md §12).
#
#   tests/games/unity-bench.sh GAME RUN SECS [gexc flags...]     GAME: hk | muck
#
# Run it on a bench box through scratch/bench/bench.sh (no game runs on gooner). It wraps
# muck-dotnet.sh and records, in RUN:
#   meta.txt     start time (Unix ms), load, flags;
#   plog-t.txt   the player log, each line prefixed with milliseconds since the start;
#   out.txt      gexc's stderr: `[gex-gl] first present at MS` and `[gex-gl] presents/s` once a
#                second (GEX_GL_FPS: the game's own glXSwapBuffers, not compositor commits), and with
#                PROF=1 `[gex-prof] AOT x% of guest (...)` every 10 s (GEX_PROF_SHARE: which tier
#                the CPU samples hit) plus prof.txt for tools/prof-report.py.
# Summarise with tests/games/unity-bench-sum.py RUN.
#
# Env: DNC (the provider build, default STAGE/dnc), STAGE (default ~/gex-sandbox/scratch/unity: dnc/ with provider/mono.wasm and lib/, gl64/,
# edger/bin/Xwayland and edger/llvm-bin when the box has none), GEXC (default STAGE/edger/gexc), CORES (taskset list,
# default bench.sh's BENCH_CORES), PROF=1, EVERY (keep every Nth of the game's frames as PPM, GEX_PPM_GL_ONLY; default 1800),
# XNUM (X display, default 77), and anything muck-dotnet.sh reads.
set -u
H=$(cd "$(dirname "$0")" && pwd)
GAME=$1 RUN=$(realpath -m "$2") SECS=$3; shift 3
S=${STAGE:-$HOME/gex-sandbox/scratch/unity}
G=${GAMES:-$HOME/gex-sandbox}
case $GAME in
	hk) export MUCK="$G/games-linux/Hollow Knight" EXE=hollow_knight.x86_64 UNITY_RELAUNCH=1 STEAM_APPID=367520 \
		HOME_SEED=$H/seeds/hollow-knight ;;
	muck) export MUCK=$G/games-linux/Muck ;;
	*) echo "GAME: hk or muck" >&2; exit 2 ;;
esac
export GEXC=${GEXC:-$S/edger/gexc} GFX=$S/gl64 XNUM=${XNUM:-77} EVERY=${EVERY:-1800} GEX_GL_FPS=1 GEX_PPM_GL_ONLY=1
[ -d "$HOME/.dotnet/shared/Microsoft.NETCore.App/10.0.10" ] && export DOTNET_FW=$HOME/.dotnet/shared/Microsoft.NETCore.App/10.0.10
XW=()
[ -x "$S/edger/bin/Xwayland" ] && ! command -v Xwayland > /dev/null && XW=(--env "XWAYLAND=$S/edger/bin/Xwayland")
# A box without wasm-ld (edger): gooner's LLVM 22 staged in ~/toolchains/llvm-22 (STAGE/edger/llvm-bin wraps it).
if ! command -v wasm-ld > /dev/null && [ -x "$S/edger/llvm-bin/wasm-ld" ]; then
	export GEX_CLANG=$S/edger/llvm-bin/clang GEX_WASM_LD=$S/edger/llvm-bin/wasm-ld
fi
[ -n "${PROF:-}" ] && export GEX_PROF=$RUN/prof.txt GEX_PROF_SHARE=1 GEX_PERFMAP=1
export CORES=${CORES:-${BENCH_CORES:-}}
[ -n "$CORES" ] || unset CORES
rm -rf "$RUN"; mkdir -p "$RUN"
t0=$(date +%s%3N)
{ echo "start $t0 game $GAME secs $SECS flags $* cores ${CORES:-all}"; echo "host $(hostname) load $(cut -d' ' -f1-3 /proc/loadavg)";
  echo "gexc $(stat -c '%s %y' "$GEXC")"; env | grep -E '^(GEX|GEXMONO|MONO)_' | sort; } > "$RUN/meta.txt"
# The player log with times. setsid: the stamper and its tail are one group, ended by its id.
setsid bash -c '
	while [ ! -e "$1/player.log" ]; do sleep 0.2; done
	tail -n +1 -F "$1/player.log" 2> /dev/null | while IFS= read -r l; do echo "$(( $(date +%s%3N) - $2 )) $l"; done > "$1/plog-t.txt"
' _ "$RUN" "$t0" &
stamper=$!
bash "$H/muck-dotnet.sh" "${DNC:-$S/dnc}" "$RUN" "$SECS" "$@" "${XW[@]}" > "$RUN/wrap.txt" 2>&1
echo "end $(date +%s%3N) load $(cut -d' ' -f1-3 /proc/loadavg)" >> "$RUN/meta.txt"
sleep 1
kill -- -"$stamper" 2> /dev/null
python3 "$H/unity-bench-sum.py" "$RUN"
