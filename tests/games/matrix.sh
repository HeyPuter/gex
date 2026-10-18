#!/bin/bash
# The games smoke matrix (docs/games.md "Games matrix"): launch each game of
# tests/games/matrix.txt natively under gexc with a time limit, and report
# how far it got.
#
#   tests/games/matrix.sh [--time S] [--only SLUG,...] [--out DIR] [--keep]
#                         [--frames] [--no-listed] [-- GEXC-FLAGS]
#   tests/games/matrix.sh --table DIR    (DIR/summary.txt as a markdown table)
#
# Rows of kind dotnet, blocked or idea are not run; they go into the summary
# with their reason (--no-listed leaves them out).
#
# --frames keeps every written frame (DIR/SLUG/frames/f-N.png), for writing
# an input script.
#
# For every game: its log, the first and the last frame it presented
# (first.png, last.png; frames are counted by --wayland-out, every
# PPM_EVERY-th one written), and one summary line in DIR/summary.txt:
#
#   slug  arch  host  stage  frames  first  menu  seconds  end  reason
#
# host is where it ran (MATRIX_HOST, default the short hostname; bench.sh
# runs on edger/worldmachine). first and menu are the seconds from launch to
# the first presented frame and to the menu frame (`-` if not reached): they
# decide where a game's check runs (user 2026-10-14: 8 minutes or more to the
# title on gooner moves it to a bench box through bench.sh, BENCH_CLASS=check,
# unless the box is no faster), never a speed result. matrix.txt's `host=H`
# in the extra field is the host a game's check belongs on; --host H runs only
# the games that belong on H (rows without host= belong on gooner).
#
# stage is the furthest of none (no frame), frame (presented something;
# `blank` when the last frame is a single colour), menu and gameplay (the
# frame numbers matrix.txt gives for them, reached with its input script).
# end is `exit N`, `signal N` or `timeout` (still running at --time). reason
# is the last line of the log that looks like why it stopped (an unhandled
# exception, an unimplemented call, a panic, ...), for a first look; DIR/SLUG/
# excerpt holds the last 30 such lines. DIR/SLUG/focus-frame is the frame
# (and seconds) at which a placed window first had the keyboard.
#
# This is a correctness/debug tool: never read speed from it (no timing on
# gooner; fps comes from the bench box, scratch/bench/bench.sh).
#
# Env: GEXC (default target/x86/release/gexc), GAMES (Windows builds,
# default ~/gex-sandbox/games), GAMES_LINUX (default ~/gex-sandbox/games-linux),
# GEX_WINE_PREFIX (a staged prefix, tools/nt-stage-wine.sh, or stream W's
# native-Wine prefix with GEXC its gexc; each game runs on a copy of it,
# large files hard-linked, new each run unless --keep), GEX_GL_X64
# (guest GL for x86-64 Linux games: GEX_CC=gcc guest/gl/build.sh DIR),
# PPM_EVERY (default 10), WSIZE (Linux window size, default 1280x720),
# GEX_STEAM_LINUX64 (gex-steam for x86-64 Linux games), LINUX_LIBS (more
# library directories for Linux games, e.g. SDL 1.2 and
# libpng12, which gooner lacks),
# GEX_STEAM_WIN (gex-steam for Windows, guest/steam/build-win.sh OUT: its
# steam_api.dll replaces the game's, so games run with no Steam client),
# GAME_ENV (space-separated K=V for the game's environment, e.g.
# GEX_STEAM_TRACE=1).
# W3 games (kind w3): W3_DIST (a W3 Wine install, lib/wine/i386-unix/wine;
# default ~/gex-sandbox/scratch/K/gate-w3legs/dist), W3_PREFIX (a prefix made
# for it with Graphics=wayland; default the same directory's pfx; copied per
# run, never written), W3_LAUNCHER (a Windows program run first with the game
# as its arguments, e.g. 'C:\winlogon.exe' for the Win7 shell's prefix),
# WINEDEBUG (default err+all,fixme-all), W3_UNITS (a unit cache, as
# tools/wine-wasm/run.sh's: GEX_W3_UNITS=all GEX_CGEN_CACHE=W3_UNITS, and
# GEX_W3_UNITS_BUILD=1 builds the missing ones in the background for the
# next run; W3_UNITS_BUILD=0 builds none). GEX_W3_TRACE=1 names each image
# that runs on tier-1 because its unit is missing.
# Take W3_DIST and W3_PREFIX from tools/wine-wasm (`make -s paths`, or
# w3-dist.sh WINE CACHE for a Wine checkout, as the gate's W3 legs).
# W64 games (kind w64, x86-64 Windows on 64-bit native Wine): W64_DIST,
# W64_PREFIX and W64_NATIVE, as tools/wine-wasm/w64-dist.sh prints them.
# The dist runs with W64_NATIVE's files in place (the native PE core, which
# has winewayland.drv), the prefix gets the Wayland driver with emulated
# display modes (as nt-x64.sh's display cases), WINEGEX_DOOR=1, and
# WINEDLLOVERRIDES=mscoree=;mshtml= (no Mono or Gecko installer).
# Every game runs in a systemd user scope with TasksMax=TASKS_MAX (default
# 1024; MATRIX_SCOPE=0 leaves it out), so a runaway can't storm the host.
# Games run at nice MATRIX_NICE (default 10; 15 or more while a gate runs).
set -u
# MATRIX_HERE: tests/games of the tree to use (for a copy of this script run
# while the tree is edited: bash reads a running script as it goes).
here=${MATRIX_HERE:-$(cd "$(dirname "$0")" && pwd)}
root=$(cd "$here/../.." && pwd)
onlyhost= limit=300 only= out= keep=0 keepframes=0 listed=1 table= flags=(--no-cgen)
while [ $# -gt 0 ]; do
    case $1 in
        --table) table=$2; shift ;;
        --no-listed) listed=0 ;;
        --host) onlyhost=$2; shift ;;
        --time) limit=$2; shift ;;
        --only) only=,$2,; shift ;;
        --out) out=$2; shift ;;
        --keep) keep=1 ;;
        --frames) keepframes=1 ;;
        --) shift; flags=("$@"); break ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
    shift
done
# --table: the summary as a markdown table, for docs/games.md.
if [ -n "$table" ]; then
    echo '| game | arch | host | stage | frames | first frame s | menu s | secs | end | first failure / reason |'
    echo '|---|---|---|---|---|---|---|---|---|---|'
    tail -n +2 "$table/summary.txt" | while read -r slug arch h stage frames tf tm secs end rest; do
        case $end in exit|signal) end="$end ${rest%% *}"; rest=${rest#* } ;; esac
        printf '| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n' "$slug" "$arch" "$h" "$stage" "$frames" "$tf" "$tm" "$secs" "$end" "$(printf %s "$rest" | sed 's/|/\\|/g')"
    done
    exit 0
fi
gexc=${GEXC:-$root/target/x86/release/gexc}
games=${GAMES:-$HOME/gex-sandbox/games}
games_linux=${GAMES_LINUX:-$HOME/gex-sandbox/games-linux}
prefix=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
w3dist=${W3_DIST:-$HOME/gex-sandbox/scratch/K/gate-w3legs/dist}
w3prefix=${W3_PREFIX:-$(dirname "$w3dist")/pfx}
w64dist=${W64_DIST:-} w64prefix=${W64_PREFIX:-} w64native=${W64_NATIVE:-}
scope=()
[ "${MATRIX_SCOPE:-1}" = 1 ] && scope=(systemd-run --user --scope -q -p TasksMax=${TASKS_MAX:-1024})
gl64=${GEX_GL_X64:-}
every=${PPM_EVERY:-10}
host=${MATRIX_HOST:-$(hostname -s)}
# MATRIX_STATE: the games' and prefixes' run copies; on the games'
# filesystem (hard links).
state=${MATRIX_STATE:-${TMPDIR:-/tmp}/gex-games-$(id -u)/matrix}
out=${out:-$state/out}
[ -x "$gexc" ] || { echo "no $gexc" >&2; exit 2; }
mkdir -p "$out" "$state"
summary=$out/summary.txt
[ -f "$summary" ] || printf '%-18s %-6s %-12s %-9s %7s %6s %6s %6s %-10s %s\n' slug arch host stage frames first menu secs end reason > "$summary"

# The W64 dist with W64_NATIVE's files in place (nt-x64.sh's NATIVE=): the
# Unix side copied (the loader finds the PE DLLs beside itself), links to
# the rest. Made once per matrix run, in the state directory.
w64run=
w64_overlay() {
    [ -n "$w64run" ] && return 0
    [ -x "$w64dist/lib/wine/x86_64-unix/wine" ] || { echo "no W64 dist at '$w64dist' (W64_DIST)" >&2; return 1; }
    [ -d "$w64prefix/drive_c" ] || { echo "no W64 prefix at '$w64prefix' (W64_PREFIX)" >&2; return 1; }
    local o=$state/w64dist f
    rm -rf "$o"; mkdir -p "$o/lib/wine/x86_64-windows"
    cp -a "$w64dist/lib/wine/x86_64-unix" "$o/lib/wine/"
    ln -s "$w64dist/share" "$o/share"; ln -s ../../share "$o/lib/wine/share"
    for f in "$w64dist"/lib/wine/x86_64-windows/*; do ln -s "$f" "$o/lib/wine/x86_64-windows/"; done
    if [ -n "$w64native" ]; then
        [ -f "$w64native/winepe.wasm" ] || { echo "no winepe.wasm in $w64native (W64_NATIVE)" >&2; return 1; }
        for f in "$w64native"/*; do rm -f "$o/lib/wine/x86_64-windows/${f##*/}"; cp "$f" "$o/lib/wine/x86_64-windows/"; done
    fi
    w64run=$o
}

trim() { local s=$1; s=${s#"${s%%[![:space:]]*}"}; s=${s%"${s##*[![:space:]]}"}; printf %s "$s"; }

# The newest frame index written under $1 (f-NNNN.ppm).
newest() { ls "$1" 2>/dev/null | sed -n 's/^f-0*\([0-9][0-9]*\)\.ppm$/\1/p' | sort -n | tail -1; }

# Whether a PPM is one colour (a cleared window): sample a grid.
blank() {
    python3 - "$1" <<'EOF'
import sys
d = open(sys.argv[1], 'rb').read()
parts, pos = [], 0
while len(parts) < 4:
    while d[pos:pos+1].isspace(): pos += 1
    if d[pos:pos+1] == b'#':
        while d[pos:pos+1] != b'\n': pos += 1
        continue
    e = pos
    while not d[e:e+1].isspace(): e += 1
    parts.append(d[pos:e]); pos = e
w, h = int(parts[1]), int(parts[2]); px = d[pos+1:]
seen = {bytes(px[3*(y*w+x):3*(y*w+x)+3]) for y in range(0, h, max(1, h//64)) for x in range(0, w, max(1, w//64))}
sys.exit(0 if len(seen) <= 1 else 1)
EOF
}

reason_of() {
    grep -aE 'unhandled|Unhandled|unimplemented|not implemented|illegal instruction|panicked|fatal|FATAL|abort|segfault|SIGSEGV|page fault|exception|err:|error:|Error' "$1" \
        | grep -avE 'gex-nt: stats|fixme:' | tail -1 | cut -c1-160
}
excerpt_of() {
    grep -aE 'unhandled|Unhandled|unimplemented|not implemented|illegal instruction|panicked|fatal|FATAL|abort|segfault|SIGSEGV|page fault|exception|err:|error:|Error|wasm trap|gexc:' "$1" \
        | grep -avE 'gex-nt: stats|fixme:' | tail -30 | cut -c1-300
}

# matrix.txt on fd 3, and every command in the loop on /dev/null: a
# Windows run used to eat the next rows from the loop's stdin.
while IFS='|' read -r -u 3 slug kind dir prog args menu play input extra; do
    slug=$(trim "$slug")
    case $slug in ''|'#'*) continue ;; esac
    [ -n "$only" ] && [[ $only != *",$slug,"* ]] && continue
    if [ -n "$onlyhost" ]; then
        want=gooner; [[ $extra =~ (^|[[:space:]])host=([^[:space:]]+) ]] && want=${BASH_REMATCH[2]}
        [ "$want" = "$onlyhost" ] || continue
    fi
    kind=$(trim "$kind") dir=$(trim "$dir") prog=$(trim "$prog") args=$(trim "$args")
    menu=$(trim "$menu") play=$(trim "$play") input=$(trim "$input") extra=$(trim "${extra:-}")
    # `@path`: run from the game's directory rather than the program's
    # (Baba Is You's launcher does `cd $GAMEDIR; ./bin64/Chowdren`).
    fromroot=0
    case $prog in @*) prog=${prog#@} fromroot=1 ;; esac
    case $kind in
    dotnet|blocked|idea)
        [ $listed = 1 ] && printf '%-18s %-6s %-12s %-9s %7s %6s %6s %6s %-10s %s\n' "$slug" - - "$kind" 0 - - 0 - "$prog" | tee -a "$summary"
        continue ;;
    esac
    case $kind in linux*) gdir=$games_linux/$dir ;; *) gdir=$games/$dir ;; esac
    exe=$gdir/$prog
    g=$out/$slug
    rm -rf "$g"; mkdir -p "$g/frames"
    if [ ! -e "$exe" ]; then
        printf '%-18s %-6s %-12s %-9s %7s %6s %6s %6s %-10s %s\n' "$slug" - "$host" none 0 - - 0 missing "no $exe" | tee -a "$summary"
        continue
    fi
    case $(file -bL "$exe") in
        *PE32+*) arch=x64 ;; *PE32*) arch=i386 ;;
        *ELF\ 64-bit*) arch=x64 ;; *ELF\ 32-bit*) arch=i386 ;; *) arch=? ;;
    esac
    # The game runs from a copy of its directory: hard links for the large
    # files, real copies of the small ones (configuration a game rewrites in
    # place), so nothing a run writes reaches the original.
    ov=$state/game-$slug
    rm -rf "$ov"
    if ! cp -al "$gdir" "$ov" 2>"$g/cp.err"; then
        # Hard links need the state directory (MATRIX_STATE, TMPDIR) on the
        # games' filesystem; a full copy would be gigabytes per game.
        rm -rf "$ov"
        printf '%-18s %-6s %-12s %-9s %7s %6s %6s %6s %-10s %s\n' "$slug" - "$host" none 0 - - 0 setup "cannot hard-link $gdir into $state (put MATRIX_STATE on its filesystem)" | tee -a "$summary"
        continue
    fi
    find "$ov" -type f -size -256k -print0 | while IFS= read -r -d '' f; do cp -p "$f" "$f.gexcp" && mv "$f.gexcp" "$f"; done
    exe=$ov/$prog
    env=(GEX_PPM_EVERY=$every)
    if [ "$input" != - ] && [ -n "$input" ] && [[ $input == *.wl ]]; then
        # Linux and W3 games: the compositor's scripted input (GEX_WL_INPUT,
        # timed in seconds from a frame).
        env+=("GEX_WL_INPUT=$here/$input")
    elif [ "$input" != - ] && [ -n "$input" ]; then
        env+=("GEX_NT_INPUT=$(sed -e 's/#.*//' -e '/^[[:space:]]*$/d' "$here/$input" | tr -d ' ' | paste -sd';')")
    fi
    # shellcheck disable=SC2206
    # `seed=DIR@GUEST` in the extra flags: a fresh copy of tests/games/DIR
    # mounted at GUEST (`~` is the user's home), so a game that keeps state
    # in a fixed place (Baba Is You's ~/.local/share/Baba_Is_You, found with
    # getpwuid rather than $HOME) starts from known settings and never
    # writes the real home.
    gflags=()
    nseed=0
    for x in $extra; do
        case $x in
        seed=*@*)
            src=${x#seed=}; src=${src%@*}; dst=${x#*@}
            case $dst in "~"*) dst=$HOME${dst#"~"} ;; esac
            sd=$state/seed-$slug/$nseed; nseed=$((nseed + 1))
            rm -rf "$sd"; mkdir -p "$sd"; cp -r "$here/$src/." "$sd/"
            gflags+=(--bind "$sd=$dst") ;;
        stub=*@*)
            # `stub=NAME@PATH`: tests/games/stubs' NAME, built for the game's
            # arch, in place of PATH (relative to the game's directory) in
            # the run copy; the original stays beside it as .orig.
            sn=${x#stub=}; sn=${sn%@*}; sp=$ov/${x#*@}
            [ -e "$sp" ] && mv "$sp" "$sp.orig"
            # MATRIX_STUBS=DIR: prebuilt DIR/NAME-ARCH.dll (a host without
            # mingw-w64, e.g. a bench box).
            if [ -n "${MATRIX_STUBS:-}" ] && [ -f "$MATRIX_STUBS/$sn-$arch.dll" ]; then
                cp "$MATRIX_STUBS/$sn-$arch.dll" "$sp"
            else
                "$here/stubs/build.sh" "$sn" "$arch" "$sp" >"$g/stub-$sn.log" 2>&1 ||
                    echo "$slug: stub $sn did not build (see $g/stub-$sn.log)" >&2
            fi ;;
        host=*|pe=*|page=*|eventlog=*) ;;
        *) gflags+=("$x") ;;
        esac
    done
    run=("$gexc" run "${flags[@]}" "${gflags[@]}" --gl --wayland-out "$g/frames/f")
    for e in ${GAME_ENV:-}; do run+=(--env "$e"); done
    pre=()
    if [[ $kind == linux* ]]; then
        [ -n "$gl64" ] || { echo "$slug: set GEX_GL_X64 (guest/gl/build.sh)" >&2; continue; }
        mkdir -p "$state/home-$slug"
        run+=(--wayland "$state/wl-$slug" --wayland-size "${WSIZE:-1280x720}"
              --env LD_LIBRARY_PATH="$gl64:$(dirname "$exe"):$ov/usr/lib${LINUX_LIBS:+:$LINUX_LIBS}"
              --env SDL_VIDEODRIVER=wayland --env SDL_AUDIODRIVER=dummy --env HOME="$state/home-$slug"
              --env XDG_RUNTIME_DIR="$state"
              # No session bus: without one, libdbus autolaunches dbus-launch,
              # which waits on the X display (Baba Is You never got past it).
              --env DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent/gex-no-bus)
        # gex-steam (guest/steam/build.sh OUT g++) in place of the game's
        # libsteam_api.so when GEX_STEAM_LINUX64 is set.
        if [ -n "${GEX_STEAM_LINUX64:-}" ] && [ -e "$(dirname "$exe")/libsteam_api.so" ]; then
            mv "$(dirname "$exe")/libsteam_api.so" "$(dirname "$exe")/libsteam_api.so.orig"
            cp "$GEX_STEAM_LINUX64/libsteam_api.so" "$(dirname "$exe")/libsteam_api.so"
        fi
        # linux-x11: an X11-only program (SDL 1.2), on Xwayland run inside
        # the guest as a client of gex's compositor (no Xvfb).
        if [ "$kind" = linux-x11 ]; then
            run+=(--env SDL_VIDEODRIVER=x11)
            xn=$(( 60 + $(printf %s "$slug" | cksum | cut -c1-2) % 30 ))
            # Xwayland's lock and socket land in the host's /tmp: a stale
            # pair from an earlier run would look like a running server.
            rm -f "/tmp/.X$xn-lock" "/tmp/.X11-unix/X$xn"
            pre=(/bin/sh "$here/xwayland-inguest.sh" "$xn" "$g/xwayland.log")
            # Xwayland's keymap compiles cached (xkbcomp-cache.sh).
            run+=(--bind "$here/xkbcomp-cache.sh=/usr/bin/xkbcomp" --bind "/usr/bin/xkbcomp=/usr/bin/xkbcomp.real")
            # No window manager runs, so no window gets the X input focus
            # and keys reach nothing: xfocus.c gives it to the largest
            # top-level window (NOFOCUS=1 leaves it out).
            if [ -z "${NOFOCUS:-}" ] && cc -O1 -o "$state/xfocus" "$here/xfocus.c" -lX11 2>/dev/null; then
                run+=(--env GEX_XFOCUS="$state/xfocus")
            fi
        fi
    else
        # Windows games (w3 and pe).
        # gex-steam's steam_api[64].dll in place of the game's when
        # GEX_STEAM_WIN is set; the original stays beside it as .orig, which
        # gex-steam reads for the interface versions of old SDKs.
        if [ -n "${GEX_STEAM_WIN:-}" ]; then
            for d in steam_api.dll:i386 steam_api64.dll:x64; do
                f=$(dirname "$exe")/${d%%:*}
                [ -e "$f" ] || continue
                mv "$f" "$f.orig"
                cp "$GEX_STEAM_WIN/${d##*:}/${d%%:*}" "$f"
            done
        fi
        pfx=$state/prefix-$slug
      if [ "$kind" = w64 ]; then
        w64_overlay || continue
        # `pe=DLL[,DLL...]`: those DLLs stay Wine's PE build for this game
        # instead of W64_NATIVE's (OneShot's Ruby scans ucrtbase's x86 code
        # for _isatty, which the native module has none of). A per-game copy
        # of the overlay, hard-linked.
        w64g=$w64run
        if [[ $extra =~ (^|[[:space:]])pe=([^[:space:]]+) ]]; then
            w64g=$state/w64dist-$slug; rm -rf "$w64g"; cp -al "$w64run" "$w64g" || continue
            for d in ${BASH_REMATCH[2]//,/ }; do
                [ -e "$w64dist/lib/wine/x86_64-windows/$d" ] || { echo "$slug: pe=$d: not in W64_DIST" >&2; continue; }
                rm -f "$w64g/lib/wine/x86_64-windows/$d"; ln -s "$w64dist/lib/wine/x86_64-windows/$d" "$w64g/lib/wine/x86_64-windows/"
            done
        fi
        if [ $keep = 0 ] || [ ! -d "$pfx/drive_c" ]; then
            rm -rf "$pfx"; cp -a "$w64prefix" "$pfx"
            printf '\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="wayland"\n' >> "$pfx/user.reg"
            printf '\n[Software\\\\Wine\\\\X11 Driver] 1791289000\n"EmulateModeset"="Y"\n' >> "$pfx/user.reg"
            [ -e "$pfx/drive_c/windows/system32/winewayland.drv" ] ||
                cp "$w64g/lib/wine/x86_64-windows/winewayland.drv" "$pfx/drive_c/windows/system32/"
        fi
        mkdir -p "$state/wl-$slug"
        run+=(--env WINEPREFIX="$pfx" --env WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}" --env WINEGEX_DOOR=1
              --env 'WINEDLLOVERRIDES=mscoree=;mshtml='
              --wayland "$state/wl-$slug" --wayland-size "${WSIZE:-1280x720}")
        pre=("$w64g/lib/wine/x86_64-unix/wine")
      elif [ "$kind" = w3 ]; then
        # W3: Wine's own loader, as a program of the guest. The prefix is a
        # full copy: wineserver and the registry rewrite files in it.
        [ -x "$w3dist/lib/wine/i386-unix/wine" ] || { echo "$slug: no W3 dist at $w3dist (W3_DIST)" >&2; continue; }
        if [ $keep = 0 ] || [ ! -d "$pfx/drive_c" ]; then
            rm -rf "$pfx"; cp -a "$w3prefix" "$pfx"
        fi
        mkdir -p "$state/wl-$slug"
        run+=(--env WINEPREFIX="$pfx" --env WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
              --wayland "$state/wl-$slug" --wayland-size "${WSIZE:-1280x720}")
        pre=("$w3dist/lib/wine/i386-unix/wine")
        [ -n "${W3_UNITS:-}" ] && env+=(GEX_W3_UNITS=all GEX_CGEN_CACHE="$W3_UNITS" GEX_W3_UNITS_BUILD=${W3_UNITS_BUILD:-1} GEX_W3_TRACE=1)
        [ -n "${W3_LAUNCHER:-}" ] && pre+=("$W3_LAUNCHER")
      else
        if [ $keep = 0 ] || [ ! -f "$pfx/.wine-version" ]; then
            rm -rf "$pfx"
            cp -al "$prefix" "$pfx" 2>/dev/null || { rm -rf "$pfx"; cp -as "$(cd "$prefix" && pwd)" "$pfx"; }
            # Small files as real copies, as for the game: the prefix may be
            # another stream's, and nothing a run rewrites may reach it.
            find "$pfx" -type f -size -256k -print0 | while IFS= read -r -d '' f; do cp -p "$f" "$f.gexcp" && mv "$f.gexcp" "$f"; done
        fi
        env+=(GEX_NT_STATS=1)
        run+=(--wine-prefix "$pfx")
      fi
    fi
    # shellcheck disable=SC2206
    argv=($args)
    t0=$(date +%s)
    if [ $fromroot = 1 ]; then cwd=$ov rel=./$prog; else cwd=$(dirname "$exe") rel=./$(basename "$exe"); fi
    (cd "$cwd" && exec env "${env[@]}" "${scope[@]}" timeout -k 20 "$limit" nice -n "${MATRIX_NICE:-10}" "${run[@]}" "${pre[@]}" "$rel" "${argv[@]}") \
        < /dev/null 3<&- > "$g/log" 2>&1 &
    pid=$!
    first= tfirst= tmenu=
    while kill -0 $pid 2>/dev/null; do
        # Keep the first frame and the newest one (or, with --frames, all
        # of them, as PNGs).
        n=$(newest "$g/frames")
        [ -n "$n" ] && [ -z "$tfirst" ] && tfirst=$(( $(date +%s) - t0 ))
        [ -n "$n" ] && [ -z "$tmenu" ] && [ "$menu" != - ] && [ "$n" -ge "$menu" ] && tmenu=$(( $(date +%s) - t0 ))
        # The frame at which a placed window first had the keyboard: where an
        # input script's clock (`frames N`) should start.
        [ -n "$n" ] && [ ! -f "$g/focus-frame" ] && grep -aq '^\[wl-place\].* focus' "$g/log" &&
            echo "$n $(( $(date +%s) - t0 ))" > "$g/focus-frame"
        if [ -n "$n" ] && [ $keepframes = 1 ]; then
            for f in $(ls "$g/frames" | grep '\.ppm$' | grep -v "^f-0*$n\.ppm$"); do
                python3 "$here/ppm2png.py" "$g/frames/$f" "$g/frames/${f%.ppm}.png" && rm -f "$g/frames/$f"
            done
        elif [ -n "$n" ]; then
            [ -z "$first" ] && first=$(ls "$g/frames" | grep '\.ppm$' | head -1) && cp "$g/frames/$first" "$g/first.ppm"
            ls "$g/frames" | grep '\.ppm$' | grep -v "^f-0*$n\.ppm$" | sed "s|^|$g/frames/|" | xargs -r rm -f
        fi
        sleep 2
    done
    wait $pid; rc=$?
    secs=$(( $(date +%s) - t0 ))
    case $rc in 124|137) end=timeout ;; 0) end="exit 0" ;; *) if [ $rc -gt 128 ]; then end="signal $((rc - 128))"; else end="exit $rc"; fi ;; esac
    n=$(newest "$g/frames")
    stage=none frames=0
    if [ -n "$n" ]; then
        frames=$n stage=frame
        last=$g/frames/$(ls "$g/frames" | grep '\.ppm$' | tail -1)
        blank "$last" && stage=blank
        python3 "$here/ppm2png.py" "$last" "$g/last.png" >/dev/null 2>&1
        [ -f "$g/first.ppm" ] && python3 "$here/ppm2png.py" "$g/first.ppm" "$g/first.png" >/dev/null 2>&1
        rm -f "$g/first.ppm"
        [ "$menu" != - ] && [ "$n" -ge "$menu" ] && stage=menu
        [ "$play" != - ] && [ "$n" -ge "$play" ] && stage=gameplay
    fi
    if [ $keepframes = 1 ]; then
        for f in $(ls "$g/frames" | grep '\.ppm$'); do
            python3 "$here/ppm2png.py" "$g/frames/$f" "$g/frames/${f%.ppm}.png" && rm -f "$g/frames/$f"
        done
    else
        rm -rf "$g/frames"
    fi
    [ -n "${xn:-}" ] && rm -f "/tmp/.X$xn-lock" "/tmp/.X11-unix/X$xn"
    xn=
    reason=$(reason_of "$g/log")
    excerpt_of "$g/log" > "$g/excerpt"
    printf '%-18s %-6s %-12s %-9s %7s %6s %6s %6s %-10s %s\n' "$slug" "$arch" "$host" "$stage" "$frames" "${tfirst:--}" "${tmenu:--}" "$secs" "$end" "${reason:--}" | tee -a "$summary"
done 3< "$here/matrix.txt" < /dev/null
