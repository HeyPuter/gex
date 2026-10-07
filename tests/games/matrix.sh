#!/bin/bash
# The games smoke matrix (docs/games.md "Games matrix"): launch each game of
# tests/games/matrix.txt natively under gexc with a time limit, and report
# how far it got.
#
#   tests/games/matrix.sh [--time S] [--only SLUG,...] [--out DIR] [--keep]
#                         [--frames] [-- GEXC-FLAGS]
#
# --frames keeps every written frame (DIR/SLUG/frames/f-N.png), for writing
# an input script.
#
# For every game: its log, the first and the last frame it presented
# (first.png, last.png; frames are counted by --wayland-out, every
# PPM_EVERY-th one written), and one summary line in DIR/summary.txt:
#
#   slug  arch  stage  frames  seconds  end  reason
#
# stage is the furthest of none (no frame), frame (presented something;
# `blank` when the last frame is a single colour), menu and gameplay (the
# frame numbers matrix.txt gives for them, reached with its input script).
# end is `exit N`, `signal N` or `timeout` (still running at --time). reason
# is the last line of the log that looks like why it stopped (an unhandled
# exception, an unimplemented call, a panic, ...), for a first look.
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
set -u
# MATRIX_HERE: tests/games of the tree to use (for a copy of this script run
# while the tree is edited: bash reads a running script as it goes).
here=${MATRIX_HERE:-$(cd "$(dirname "$0")" && pwd)}
root=$(cd "$here/../.." && pwd)
limit=300 only= out= keep=0 keepframes=0 flags=(--no-cgen)
while [ $# -gt 0 ]; do
    case $1 in
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
gexc=${GEXC:-$root/target/x86/release/gexc}
games=${GAMES:-$HOME/gex-sandbox/games}
games_linux=${GAMES_LINUX:-$HOME/gex-sandbox/games-linux}
prefix=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
gl64=${GEX_GL_X64:-}
every=${PPM_EVERY:-10}
state=${TMPDIR:-/tmp}/gex-games-$(id -u)/matrix
out=${out:-$state/out}
[ -x "$gexc" ] || { echo "no $gexc" >&2; exit 2; }
mkdir -p "$out" "$state"
summary=$out/summary.txt
[ -f "$summary" ] || printf '%-18s %-6s %-9s %7s %6s %-10s %s\n' slug arch stage frames secs end reason > "$summary"

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

while IFS='|' read -r slug kind dir prog args menu play input extra; do
    slug=$(trim "$slug")
    case $slug in ''|'#'*) continue ;; esac
    [ -n "$only" ] && [[ $only != *",$slug,"* ]] && continue
    kind=$(trim "$kind") dir=$(trim "$dir") prog=$(trim "$prog") args=$(trim "$args")
    menu=$(trim "$menu") play=$(trim "$play") input=$(trim "$input") extra=$(trim "${extra:-}")
    # `@path`: run from the game's directory rather than the program's
    # (Baba Is You's launcher does `cd $GAMEDIR; ./bin64/Chowdren`).
    fromroot=0
    case $prog in @*) prog=${prog#@} fromroot=1 ;; esac
    case $kind in linux*) gdir=$games_linux/$dir ;; *) gdir=$games/$dir ;; esac
    exe=$gdir/$prog
    g=$out/$slug
    rm -rf "$g"; mkdir -p "$g/frames"
    if [ ! -e "$exe" ]; then
        printf '%-18s %-6s %-9s %7s %6s %-10s %s\n' "$slug" - none 0 0 missing "no $exe" | tee -a "$summary"
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
    cp -al "$gdir" "$ov"
    find "$ov" -type f -size -256k -print0 | while IFS= read -r -d '' f; do cp -p "$f" "$f.gexcp" && mv "$f.gexcp" "$f"; done
    exe=$ov/$prog
    env=(GEX_PPM_EVERY=$every)
    if [ "$input" != - ] && [ -n "$input" ] && [[ $kind == linux* ]]; then
        # Linux games: the compositor's scripted input (GEX_WL_INPUT, timed
        # in seconds from a frame).
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
    # shellcheck disable=SC2206
    argv=($args)
    t0=$(date +%s)
    if [ $fromroot = 1 ]; then cwd=$ov rel=./$prog; else cwd=$(dirname "$exe") rel=./$(basename "$exe"); fi
    (cd "$cwd" && exec env "${env[@]}" timeout -k 20 "$limit" nice -n 10 "${run[@]}" "${pre[@]}" "$rel" "${argv[@]}") \
        > "$g/log" 2>&1 &
    pid=$!
    first=
    while kill -0 $pid 2>/dev/null; do
        # Keep the first frame and the newest one (or, with --frames, all
        # of them, as PNGs).
        n=$(newest "$g/frames")
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
    printf '%-18s %-6s %-9s %7s %6s %-10s %s\n' "$slug" "$arch" "$stage" "$frames" "$secs" "$end" "${reason:--}" | tee -a "$summary"
done < "$here/matrix.txt"
