#!/bin/bash
# Run a Windows program on 32-bit Wine (Wine's own Unix side as wasm32 on gex) with the
# dist and prefix `make -C tools/wine-wasm` builds, natively under gexc.
#
#   tools/wine-wasm/run.sh [options] EXE [ARGS...]
#   tools/wine-wasm/run.sh --page TREE [options] EXE [ARGS...]
#
# EXE is a host path. The program runs in its own directory (Z:\ maps the
# host root), on a fresh copy of the prefix, with its images and Wine's on
# function-tier units (GEX_WINE32_UNITS=all) from one unit cache.
#
# --page TREE runs it in the browser instead: headless Chromium on a bench box
# through BENCH_SH (required: a runner taking `-s DIR... -- CMD`, which syncs
# each DIR to the box; BENCH_TIMEOUT, default 20m), web/nt.html with gl=1, from TREE, a page-ready gex tree (web/node_modules,
# web/public/gex's x86 shims and Mesa: GEX_ARCH=x86 shims/{kernel,browser}/build.sh,
# web/build-mesa-wasm.sh). Two bundles: the dist at /w3 (WINE32_PAGE_DIR/wine32-KEY.tar,
# made once per dist) and the prefix at /pfx, the host's xkb data at /usr and
# EXE's directory at /g. The run tree (WINE32_PAGE_DIR/tree-NAME: hard links to
# TREE's web and shims/browser/js) and the unit cache are synced to
# the box; it runs until the program exits or --timeout (default 300 s), and
# prints the program's output, "unitless:" lines and an "== w3 page:" result
# (wine32/page-run.mjs). With --build-units the unitless images' units are built
# afterwards (wine32/units.sh) for the next run: the page builds none itself.
# WINE32_PAGE_DIR defaults to WINE32_CACHE/page; on a remote box it and the unit
# cache must be under paths the runner maps.
#
# Options:
#   --null             the null display driver (no compositor; tests)
#   --prefix DIR       copy this prefix instead of the Makefile's
#   --keep DIR         run in DIR, a prefix copy kept for the next run (made
#                      when missing); without it a temporary copy is removed
#   --dist DIR         this dist instead of the Makefile's
#   --units DIR        unit cache (GEX_CGEN_CACHE; default WINE32_CACHE/units).
#                      Unit keys include gexc's gex-cgen sources: one cache
#                      serves every gexc, each finding its own units.
#   --build-units      build missing units in the background for the next run
#   --size WxH         the compositor's output size (default 1280x720)
#   --out DIR|null     where the compositor writes frames (default null)
#   --env K=V          more environment for the program (repeatable)
#   --debug CHANNELS   WINEDEBUG (default err+all,fixme-all)
#   --timeout S        stop after S seconds (default: none)
#   --no-make          do not run make first (use what is built)
#   --gexc PATH        the x86 gexc (default GEXC, or this tree's target/x86/release/gexc)
#   --page TREE        in the page on a bench box (above)
# Make settings (WINE32_CACHE, WINE_DIR, ...) pass through the environment.
set -euo pipefail
WINE32=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$WINE32/../.." && pwd)
graphics=wayland prefix="" keep="" dist="" units="" size=1280x720 out=null debug=err+all,fixme-all tmo="" make=1 envs=()
gexc=${GEXC:-$ROOT/target/x86/release/gexc}
build_units="" page=""
# The bench box's half of --page: TREE UNITS SECS EXE [ARGS...], from the
# box's project root.
if [ "${1:-}" = --remote ]; then
  T=$(cd "$2" && pwd) U=$(cd "$3" && pwd) secs=$4; shift 4
  free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
  CP=$(free_port); VP=$(free_port)
  mkdir -p "$T/../tmp-$(basename "$T")"
  export TMPDIR=$(cd "$T/../tmp-$(basename "$T")" && pwd) GEX_WIN_CHROME_PORT=$CP GEX_WIN_VITE_PORT=$VP GEX_WEBGPU_SOFTWARE=0
  export GEX_WIN_BROWSER_MAX_LINES=200000 GEX_CGEN_CACHE=$U GEX_WIN_BROWSER_KEEP="no function-tier unit|validation"
  kill_ours() { for p in $(pgrep -f -- "--remote-debugging-port=$CP\b"); do kill $p 2>/dev/null; done; }
  trap 'kill_ours; [ -n "${np:-}" ] && kill $np 2>/dev/null; exit 143' TERM INT
  node "$T/tools/wine-wasm/wine32/page-run.mjs" "$T" "$secs" "$@" & np=$!
  wait $np; rc=$?
  kill_ours
  exit $rc
fi
while [ $# -gt 0 ]; do
  case $1 in
    --null) graphics=null ;;
    --prefix) prefix=$2; shift ;;
    --keep) keep=$2; shift ;;
    --dist) dist=$2; shift ;;
    --units) units=$2; shift ;;
    --build-units) build_units=1 ;;
    --size) size=$2; shift ;;
    --out) out=$2; shift ;;
    --env) envs+=(--env "$2"); shift ;;
    --debug) debug=$2; shift ;;
    --timeout) tmo=$2; shift ;;
    --no-make) make="" ;;
    --gexc) gexc=$2; shift ;;
    --page) page=$(realpath "$2"); shift ;;
    -h|--help) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
    --) shift; break ;;
    -*) echo "run.sh: unknown option $1" >&2; exit 2 ;;
    *) break ;;
  esac
  shift
done
exe=$(realpath "${1:?usage: run.sh [options] EXE [ARGS...]}"); shift
[ -n "$page" ] || [ -x "$gexc" ] || { echo "run.sh: no x86 gexc at $gexc (cargo build --release -p gexc --features x86, or --gexc)" >&2; exit 1; }
target=prefix; [ $graphics = null ] && target=prefix-null
[ -n "$make" ] && make -s -C "$WINE32" dist $target >&2
eval "$(make -s -C "$WINE32" paths)"
dist=${dist:-$WINE32_DIST}
[ -n "$prefix" ] || { [ $graphics = null ] && prefix=$WINE32_PREFIX_NULL || prefix=$WINE32_PREFIX; }
units=${units:-$WINE32_UNITS}
[ -d "$dist/lib/wine/i386-unix" ] || { echo "run.sh: no dist at $dist" >&2; exit 1; }
[ -f "$prefix/system.reg" ] || { echo "run.sh: no prefix at $prefix" >&2; exit 1; }
mkdir -p "$units"
if [ -n "$page" ]; then
  for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm gex_kernel.x86.wasm gex-wgpu-replay.wasm; do
    [ -f "$page/web/public/gex/$f" ] || { echo "run.sh: $page is not page-ready: no web/public/gex/$f" >&2; exit 1; }
  done
  [ -x "$page/web/node_modules/.bin/vite" ] || { echo "run.sh: $page has no web/node_modules" >&2; exit 1; }
  pg=$(realpath -m "${WINE32_PAGE_DIR:-$(dirname "$WINE32_DIST")/page}"); mkdir -p "$pg"
  name=$(basename "$exe"); name=${name%.*}
  # The dist bundle, once per dist (links dereferenced: the page has none).
  wt=$pg/wine32-$WINE32_KEY.tar
  [ -f "$wt" ] || { tar -C "$(dirname "$dist")" -h --transform "s,^$(basename "$dist"),w3," -cf "$wt.tmp" "$(basename "$dist")" && mv "$wt.tmp" "$wt"; }
  # The run tree, at one path per program, so the box's copy updates by difference.
  rt=$pg/tree-$name; rm -rf "$rt"; mkdir -p "$rt/web" "$rt/shims/browser" "$rt/tools/wine-wasm/wine32"
  for d in web/nt.html web/vite.config.js web/package.json web/src web/test web/node_modules web/public web/eject-site/vendor shims/browser/js; do
    mkdir -p "$rt/$(dirname "$d")"; cp -al "$page/$d" "$rt/$(dirname "$d")/" 2>/dev/null || cp -a "$page/$d" "$rt/$(dirname "$d")/"
  done
  rm -rf "$rt/web/public/gex/nt"; mkdir -p "$rt/web/public/gex/nt"
  cp "$WINE32/run.sh" "$rt/tools/wine-wasm/"; cp "$WINE32/wine32/page-run.mjs" "$rt/tools/wine-wasm/wine32/"
  ln "$wt" "$rt/web/public/gex/nt/w3.tar" 2>/dev/null || cp "$wt" "$rt/web/public/gex/nt/w3.tar"
  # The program's bundle: the prefix, xkb data, its directory.
  st=$(mktemp -d "$pg/stage.XXXXXX"); trap 'rm -rf "$st"' EXIT
  cp -a "$prefix" "$st/pfx"; mkdir -p "$st/usr/share/X11"; cp -rL /usr/share/X11/xkb "$st/usr/share/X11/"
  cp -al "$(dirname "$exe")" "$st/g" 2>/dev/null || cp -a "$(dirname "$exe")" "$st/g"
  # With the prefix's extended attributes (Wine's DOS attributes: a
  # desktop.ini hidden and system), which web/src/tar.js reads back.
  tar -C "$st" --xattrs --xattrs-include='user.*' -cf "$rt/web/public/gex/nt/app.tar" pfx usr g; rm -rf "$st"
  env=() ; for ((i = 1; i < ${#envs[@]}; i += 2)); do env+=("${envs[i]}"); done
  log=$pg/$name.log
  BENCH_TIMEOUT=${BENCH_TIMEOUT:-20m} "${BENCH_SH:?--page needs BENCH_SH, a bench runner}" -s "$rt" -s "$units" -- \
    env WINE32_PAGE_TARS=/gex/nt/w3.tar,/gex/nt/app.tar WINE32_PAGE_SIZE="$size" WINE32_PAGE_DEBUG="$debug" \
    WINE32_PAGE_ENV="$(IFS=,; echo "${env[*]}")" ${WINE32_PAGE_Q+WINE32_PAGE_Q="$WINE32_PAGE_Q"} \
    bash "$rt/tools/wine-wasm/run.sh" --remote "$rt" "$units" "${tmo:-300}" "$(basename "$exe")" "$@" 2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  [ -n "$build_units" ] && "$WINE32/wine32/units.sh" "$log" "$units" "$gexc" "$dist" "$(dirname "$exe")"
  echo "run.sh: log $log"
  exit $rc
fi
wl=$(mktemp -d "${TMPDIR:-/tmp}/wine32-wl.XXXXXX"); tmp=""
trap 'rm -rf "$wl" $tmp' EXIT
if [ -n "$keep" ]; then
  pfx=$(realpath -m "$keep"); [ -f "$pfx/system.reg" ] || cp -a "$prefix" "$pfx"
else
  tmp=$(mktemp -d "${TMPDIR:-/tmp}/wine32-run.XXXXXX"); pfx=$tmp/pfx; cp -a "$prefix" "$pfx"
fi
win="Z:$(printf %s "$exe" | tr / '\\')"
gl=(); [ $graphics = wayland ] && gl=(--gl)
cd "$(dirname "$exe")"
export GEX_WINE32_UNITS=${GEX_WINE32_UNITS:-all} GEX_CGEN_CACHE=$units
[ -n "$build_units" ] && export GEX_WINE32_UNITS_BUILD=1
cmd=("$gexc" run --env WINEPREFIX="$pfx" --env WINEDEBUG="$debug" "${envs[@]}" "${gl[@]}"
  --wayland "$wl/wl" --wayland-size "$size" --wayland-out "$out" "$dist/lib/wine/i386-unix/wine" "$win" "$@")
rc=0
if [ -n "$tmo" ]; then timeout -k 10 "$tmo" "${cmd[@]}" || rc=$?; else "${cmd[@]}" || rc=$?; fi
exit $rc
