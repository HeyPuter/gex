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
# list), EXE (another Unity game's executable in MUCK, e.g. hollow_knight.x86_64),
# UNITY_RELAUNCH=1 (run a PIE rebuild of that executable: unity-launcher.c; with it,
# STEAM_APPID=N writes steam_appid.txt beside it), DOTNET_FW (the .NET 10 shared framework directory),
# GUEST_LIBS=DIR (with UNITY_RELAUNCH: the launcher's interpreter is DIR's ld-linux-x86-64.so.2 and DIR
# comes first in its inherited DT_RPATH: another distribution's glibc, e.g. worldmachine's, to
# reproduce what only happens there), OVERLAY=DIR (with UNITY_RELAUNCH: DIR's files, by
# path relative to the game directory, replace or join the game's in this run's copy; the
# directories on their way become real directories of links, the game's own stay untouched),
# HOME_SEED=DIR (copied into the run's HOME first: saved settings, e.g. Unity's prefs, so a
# game skips its first-run screens).
#
# Default: batch mode, no graphics. GFX=GLDIR: with graphics, GL through
# gex-gl (GLDIR holds guest/gl/build.sh's x86-64 build) and X from an
# Xwayland run inside the guest on gex's compositor (xwayland-inguest.sh;
# no Xvfb); frames go to RUNDIR/f-*.ppm every EVERY (30) frames. The game window gets the
# input focus from tests/games/xfocus.c (no window manager runs; NOFOCUS=1 leaves it out).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath "$1"); RUN=$(realpath -m "$2"); SECS=${3:-1800}
shift 2; [ $# -gt 0 ] && shift
MUCK=${MUCK:-$HOME/gex-sandbox/games-linux/Muck}
EXE=${EXE:-Muck.x86_64}
GEXC=${GEXC:-$HERE/../../target/x86/release/gexc}
FW=${DOTNET_FW:-$(dirname "$(dotnet --list-runtimes | awk '/Microsoft.NETCore.App 10\./ {v=$2; p=$3} END {gsub(/[\[\]]/, "", p); print p "/" v "/x"}')")}
mkdir -p "$RUN/bcl" "$RUN/home"
[ -n "${HOME_SEED:-}" ] && cp -a "$HOME_SEED/." "$RUN/home/"
cp "$FW"/*.dll "$RUN/bcl/"
cp "$OUT/lib/System.Private.CoreLib.dll" "$RUN/bcl/"
# Out-of-band assemblies over the framework's (build.sh formatters: BinaryFormatter).
[ -d "$OUT/lib-extra" ] && cp "$OUT"/lib-extra/*.dll "$RUN/bcl/"
# The framework's x86-64 natives that have no wasm64 build in the provider: P/Invokes to them go to
# the guest (the provider's dl fallback), which loads them beside the BCL with the host's OpenSSL/zlib.
cp "$FW"/libSystem.Security.Cryptography.Native.OpenSsl.so "$FW"/libSystem.IO.Compression.Native.so "$RUN/bcl/"
if [ -n "${UNITY_RELAUNCH:-}" ]; then
	# The game's executable rebuilt PIE (unity-launcher.c) among links to its files.
	G="$RUN/game"; mkdir -p "$G"
	for f in "$MUCK"/*; do [ "$(basename "$f")" = "$EXE" ] || ln -sfn "$f" "$G/"; done
	LIBS=; [ -n "${GUEST_LIBS:-}" ] && LIBS=$(realpath "$GUEST_LIBS") && \
		LIBS="-Wl,--allow-shlib-undefined -Wl,--disable-new-dtags -Wl,--dynamic-linker=$LIBS/ld-linux-x86-64.so.2 -Wl,-rpath,$LIBS"
	cc -O1 -fPIE -pie -o "$G/$EXE" "$HERE/unity-launcher.c" -L"$MUCK" -l:UnityPlayer.so -Wl,-rpath,'$ORIGIN' $LIBS
	# STEAM_APPID: Steamworks' development file, so a game started outside Steam runs instead of
	# asking Steam to restart it.
	[ -n "${STEAM_APPID:-}" ] && echo "$STEAM_APPID" > "$G/steam_appid.txt"
	if [ -n "${OVERLAY:-}" ]; then
		( cd "$OVERLAY" && find . -type f ) | while read -r f; do
			f=${f#./}; d=$G
			rest=$(dirname "$f")
			# Each directory on the way: a link to the game's becomes a directory of links.
			while [ "$rest" != . ]; do
				first=${rest%%/*}
				if [ -L "$d/$first" ]; then
					src=$(readlink -f "$d/$first"); rm "$d/$first"; mkdir "$d/$first"
					for e in "$src"/* "$src"/.[!.]*; do [ -e "$e" ] && ln -s "$e" "$d/$first/"; done
				fi
				mkdir -p "$d/$first"; d=$d/$first
				[ "$rest" = "$first" ] && rest=. || rest=${rest#*/}
			done
			rm -f "$G/$f"; cp "$OVERLAY/$f" "$G/$f"
		done
	fi
	MUCK=$G
fi
cd "$MUCK"
export GEX_WASM_DLLS=libmonobdwgc-2.0.so="$OUT/provider/mono.wasm"
if [ -z "${GFX:-}" ]; then
	timeout -s KILL "$SECS" ${CORES:+taskset -c "$CORES"} \
		"$GEXC" run --mem64 "$@" --env HOME="$RUN/home" --env GEXMONO_ASSEMBLIES="$RUN/bcl" \
		./$EXE -batchmode -nographics -logFile "$RUN/player.log" > "$RUN/out.txt" 2>&1 || echo "exit $?"
else
	N=${XNUM:-71}
	FOCUS=
	if [ -z "${NOFOCUS:-}" ]; then
		cc -O1 -o "$RUN/xfocus" "$HERE/xfocus.c" -lX11 && FOCUS="--env GEX_XFOCUS=$RUN/xfocus"
	fi
	[ -e /tmp/.X$N-lock ] && { echo "display :$N is taken (XNUM)"; exit 1; }
	GEX_FPS=1 GEX_PPM_EVERY=${EVERY:-30} timeout -s KILL "$SECS" ${CORES:+taskset -c "$CORES"} \
		"$GEXC" run --mem64 "$@" --gl --wayland "${XDG_RUNTIME_DIR:-/tmp}/muckwl-$$" --wayland-size 800x600 --wayland-out "$RUN/f" \
		--env LD_LIBRARY_PATH="$(realpath "$GFX")" --env HOME="$RUN/home" --env GEXMONO_ASSEMBLIES="$RUN/bcl" $FOCUS \
		/bin/sh "$HERE/xwayland-inguest.sh" $N "$RUN/xwayland.log" \
		./$EXE -logFile "$RUN/player.log" -screen-width 800 -screen-height 600 -screen-fullscreen 0 \
		> "$RUN/out.txt" 2>&1 || echo "exit $?"
	# Xwayland's lock and socket node are host files; remove only this display's.
	rm -f /tmp/.X$N-lock /tmp/.X11-unix/X$N
fi
tail -5 "$RUN/player.log"
