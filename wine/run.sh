#!/bin/bash
# Run a Windows program on Wine under gexc: a 64-bit Wine install (--dist,
# wine64-dist.sh's) or 32-bit Wine (Wine's own Unix side as wasm32 on gex)
# with the dist and prefix `make -C wine` builds.
#
#   wine/run.sh [options] EXE [ARGS...]
#   wine/run.sh --page TREE [options] EXE [ARGS...]
#
# EXE is a host path. The program runs in its own directory (Z:\ maps the
# host root), on a fresh copy of the prefix, with its images and Wine's on
# function-tier units (GEX_WINE32_UNITS=all) from one unit cache.
#
# --page TREE runs it in the browser instead: headless Chromium on a bench box
# through BENCH_SH (required: a runner taking `-s DIR... -- CMD`, which syncs
# each DIR to the box; BENCH_TIMEOUT, default 20m), page/nt.html with gl=1, from TREE, a page-ready gex tree (page/node_modules,
# page/public/gex's x86 shims and Mesa: GEX_ARCH=x86 shims/{kernel,browser}/build.sh,
# ports/mesa/build.sh). Two bundles: the dist at /wine32 (WINE32_PAGE_DIR/wine32-KEY.tar,
# made once per dist) and the prefix at /pfx, the host's xkb data at /usr and
# EXE's directory at /g. The run tree (WINE32_PAGE_DIR/tree-NAME: hard links to
# TREE's web and hosts/browser/js) and the unit cache are synced to
# the box; it runs until the program exits or --timeout (default 300 s), and
# prints the program's output, "unitless:" lines and an "== wine32 page:" result
# (wine/page/page-run.mjs). With --build-units the unitless images' units are built
# afterwards (wine32/units.sh) for the next run: the page builds none itself.
# WINE32_PAGE_DIR defaults to WINE32_CACHE/page; on a remote box it and the unit
# cache must be under paths the runner maps.
# With --dist a 64-bit Wine install (wine64-dist.sh's: lib/wine/x86_64-unix)
# it runs that, natively or in the page (at /wine), on --prefix (required:
# its WINE64_PREFIX, copied) with --null's driver or Wayland, and without
# doors; a dist with lib/wine/i386-windows (WINE64_WOW64=1) runs 32-bit
# programs in the 64-bit process (WoW64). No make then; units default to
# units (natively) or page-units (the page's) beside the dist. --native's
# core goes over a linked copy of the dist, kept beside it (native-KEY).
#
# Options:
#   --null             the null display driver (no compositor; tests)
#   --prefix DIR       copy this prefix instead of the Makefile's
#   --native DIR       (64-bit Wine) the native PE core (wine64-dist.sh's
#                      WINE64_NATIVE) over the dist's x86_64-windows
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
WINE32=$(cd "$(dirname "$0")" && pwd); ROOT=$(cd "$WINE32/.." && pwd)
graphics=wayland prefix="" native="" keep="" dist="" units="" size=1280x720 out=null debug=err+all,fixme-all tmo="" make=1 envs=()
gexc=${GEXC:-$ROOT/target/x86/release/gexc}
build_units="" page=""
# The bench box's half of --page: TREE UNITS SECS EXE [ARGS...], from the
# box's project root.
if [ "${1:-}" = --remote ]; then
  T=$(cd "$2" && pwd) U=$(cd "$3" && pwd) secs=$4; shift 4
  free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
  CP=$(free_port); VP=$(free_port)
  # A short TMPDIR, as Chromium's singleton socket goes under it and a Unix
  # socket's path ends at 107 bytes (a tree's own deep path can pass that)
  TD=/tmp/wpg-$(printf %s "$T" | sha256sum | cut -c1-12); rm -rf "$TD"; mkdir -p "$TD"
  export TMPDIR=$TD GEX_WIN_CHROME_PORT=$CP GEX_WIN_VITE_PORT=$VP GEX_WEBGPU_SOFTWARE=0
  export GEX_WIN_BROWSER_MAX_LINES=200000 GEX_CGEN_CACHE=$U GEX_WIN_BROWSER_KEEP="no function-tier unit|validation"
  kill_ours() { for p in $(pgrep -f -- "--remote-debugging-port=$CP\b"); do kill $p 2>/dev/null; done; }
  trap 'kill_ours; [ -n "${np:-}" ] && kill $np 2>/dev/null; rm -rf "$TD"; exit 143' TERM INT
  node "$T/wine/page/page-run.mjs" "$T" "$secs" "$@" & np=$!
  wait $np; rc=$?
  kill_ours; rm -rf "$TD"
  exit $rc
fi
while [ $# -gt 0 ]; do
  case $1 in
    --null) graphics=null ;;
    --prefix) prefix=$2; shift ;;
    --native) native=$(realpath "$2"); shift ;;
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
# A 64-bit Wine dist (--dist with lib/wine/x86_64-unix: wine64-dist.sh's)
# runs as it is, with its own prefix: no make.
w64=""; [ -n "$dist" ] && [ -f "$dist/lib/wine/x86_64-unix/wine" ] && w64=1
if [ -n "$w64" ]; then
  [ -n "$prefix" ] || { echo "run.sh: a 64-bit Wine dist needs --prefix (its WINE64_PREFIX)" >&2; exit 1; }
  dist=$(realpath "$dist")
  WINE32_KEY=w64s-$({ echo "$dist $native"; find -L "$dist" ${native:+"$native"} -type f -printf '%T@ %s %p\n' | sort; } | sha256sum | cut -c1-12)
  WINE32_DIST=$dist
  units=${units:-$(dirname "$dist")/$([ -n "$page" ] && echo page-units || echo units)}
else
  [ -n "$make" ] && make -s -C "$WINE32" dist $target >&2
  eval "$(make -s -C "$WINE32" paths)"
  dist=${dist:-$WINE32_DIST}
  [ -n "$prefix" ] || { [ $graphics = null ] && prefix=$WINE32_PREFIX_NULL || prefix=$WINE32_PREFIX; }
  units=${units:-$WINE32_UNITS}
  [ -d "$dist/lib/wine/i386-unix" ] || { echo "run.sh: no dist at $dist" >&2; exit 1; }
fi
[ -f "$prefix/system.reg" ] || { echo "run.sh: no prefix at $prefix" >&2; exit 1; }
mkdir -p "$units"
if [ -n "$page" ]; then
  # (Mesa's gex-wgpu-replay.wasm not for a --null run: no GL there)
  nogl=""; [ $graphics = null ] && nogl=1
  for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm gex_kernel.x86.wasm $([ -n "$nogl" ] || echo gex-wgpu-replay.wasm); do
    [ -f "$page/page/public/gex/$f" ] || { echo "run.sh: $page is not page-ready: no page/public/gex/$f" >&2; exit 1; }
  done
  [ -x "$page/page/node_modules/.bin/vite" ] || { echo "run.sh: $page has no page/node_modules" >&2; exit 1; }
  pg=$(realpath -m "${WINE32_PAGE_DIR:-$(dirname "$WINE32_DIST")/page}"); mkdir -p "$pg"
  name=$(basename "$exe"); name=${name%.*}
  # The dist bundle, once per dist (links dereferenced: the page has none).
  # A 64-bit Wine's at /wine (its loader is lib/wine/x86_64-unix/wine),
  # 32-bit Wine's at /wine32.
  mnt=wine32; [ -n "$w64" ] && mnt=wine
  wt=$pg/wine32-$WINE32_KEY.tar
  if [ -n "$w64" ]; then
    # A WoW64 dist holds both PE sides, and the page keeps every file in the
    # global kernel's memory, which ends at 4 GiB: the bundle's PE files
    # without their debug information and without the import libraries
    # (.a), on a linked copy (the dist itself is left as it is).
    # The copy is kept beside the tar: units are built from the files the page has.
    ds=$pg/dist-$WINE32_KEY
    [ -f "$wt" ] && [ -d "$ds/$mnt" ] || {
      rm -rf "$ds" "$wt"; mkdir -p "$ds"; cp -rlL "$dist" "$ds/$mnt"
      find "$ds/$mnt/lib/wine" -name '*.a' -delete
      # the native core's files in place of the PE ones (removed first: links)
      [ -z "$native" ] || for f in "$native"/*; do rm -f "$ds/$mnt/lib/wine/x86_64-windows/${f##*/}"; cp "$f" "$ds/$mnt/lib/wine/x86_64-windows/"; done
      find "$ds/$mnt/lib/wine" -path '*-windows/*' -type f ! -name '*.a' -print0 |
        xargs -0 -r -P"$(nproc)" -n 32 sh -c 'for f; do llvm-strip --strip-debug -o "$f.s" "$f" 2>/dev/null && mv "$f.s" "$f" || rm -f "$f.s"; done' sh
      tar -C "$ds" -cf "$wt.tmp" "$mnt" && mv "$wt.tmp" "$wt"; }
    udist=$ds/$mnt
  else
    [ -f "$wt" ] || { tar -C "$(dirname "$dist")" -h --transform "s,^$(basename "$dist"),$mnt," -cf "$wt.tmp" "$(basename "$dist")" && mv "$wt.tmp" "$wt"; }
  fi
  # The run tree, at one path per program, so the box's copy updates by difference.
  rt=$pg/tree-$name; rm -rf "$rt"; mkdir -p "$rt/page" "$rt/hosts/browser" "$rt/wine/page" "$rt/test/lib"
  for d in page/nt.html page/vite.config.js page/package.json page/src page/runtime/src wine/page/browser-harness.mjs test/lib/cdp.mjs page/node_modules page/public page/vendor/zstddec hosts/browser/js; do
    mkdir -p "$rt/$(dirname "$d")"; cp -al "$page/$d" "$rt/$(dirname "$d")/" 2>/dev/null || cp -a "$page/$d" "$rt/$(dirname "$d")/"
  done
  rm -rf "$rt/page/public/gex/nt"; mkdir -p "$rt/page/public/gex/nt"
  cp "$WINE32/run.sh" "$rt/wine/"; cp "$WINE32/page/page-run.mjs" "$rt/wine/page/"
  ln "$wt" "$rt/page/public/gex/nt/wine32.tar" 2>/dev/null || cp "$wt" "$rt/page/public/gex/nt/wine32.tar"
  # The program's bundle: the prefix, xkb data, its directory.
  st=$(mktemp -d "$pg/stage.XXXXXX"); trap 'rm -rf "$st"' EXIT
  cp -a "$prefix" "$st/pfx"
  # 64-bit Wine: the prefix's driver as --null asks (its prefix keeps one)
  if [ -n "$w64" ]; then
    gd=wayland; [ $graphics = null ] && gd=null
    printf '\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="%s"\n' $gd >> "$st/pfx/user.reg"
    # wineboot's copies of the builtin PE files in system32 and syswow64,
    # links to the dist's instead (the loader takes the builtin either way):
    # the same bytes would be in the page's memory twice
    for a in system32:x86_64 syswow64:i386; do
      d=$st/pfx/drive_c/windows/${a%%:*}; b=$dist/lib/wine/${a#*:}-windows
      [ -d "$d" ] && [ -d "$b" ] || continue
      find "$d" -type f | while IFS= read -r f; do
        n=${f##*/}; [ -f "$b/$n" ] && cmp -s "$f" "$b/$n" && ln -sfn "/$mnt/lib/wine/${a#*:}-windows/$n" "$f" || true
      done
    done
  fi
  mkdir -p "$st/usr/share/X11"; cp -rL /usr/share/X11/xkb "$st/usr/share/X11/"
  cp -al "$(dirname "$exe")" "$st/g" 2>/dev/null || cp -a "$(dirname "$exe")" "$st/g"
  # With the prefix's extended attributes (Wine's DOS attributes: a
  # desktop.ini hidden and system), which page/src/tar.js reads back.
  tar -C "$st" --xattrs --xattrs-include='user.*' -cf "$rt/page/public/gex/nt/app.tar" pfx usr g; rm -rf "$st"
  env=() ; for ((i = 1; i < ${#envs[@]}; i += 2)); do env+=("${envs[i]}"); done
  # 64-bit Wine: its 32-bit programs in the 64-bit process (WoW64) when
  # the dist has the i386 side
  if [ -n "$w64" ]; then
    [ -d "$dist/lib/wine/i386-windows" ] && env+=(WINEGEX_WOW64=1)
  fi
  log=$pg/$name.log
  BENCH_TIMEOUT=${BENCH_TIMEOUT:-20m} "${BENCH_SH:?--page needs BENCH_SH, a bench runner}" -s "$rt" -s "$units" -- \
    env WINE32_PAGE_TARS=/gex/nt/wine32.tar,/gex/nt/app.tar WINE32_PAGE_SIZE="$size" WINE32_PAGE_DEBUG="$debug" \
    WINE32_PAGE_EXE=/$mnt/lib/wine/$([ -n "$w64" ] && echo x86_64 || echo i386)-unix/wine ${nogl:+WINE32_PAGE_GL=0} \
    WINE32_PAGE_ENV="$(IFS=,; echo "${env[*]}")" ${WINE32_PAGE_Q+WINE32_PAGE_Q="$WINE32_PAGE_Q"} \
    bash "$rt/wine/run.sh" --remote "$rt" "$units" "${tmo:-300}" "$(basename "$exe")" "$@" 2>&1 | tee "$log"
  rc=${PIPESTATUS[0]}
  [ -n "$build_units" ] && "$WINE32/units.sh" "$log" "$units" "$gexc" "${udist:-$dist}" "$(dirname "$exe")" ${w64:+"$prefix"}
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
loader=$dist/lib/wine/i386-unix/wine
if [ -n "$w64" ]; then
  # the prefix's driver as --null asks (its prefix keeps one)
  gd=wayland; [ $graphics = null ] && gd=null
  [ "$(grep '^"Graphics"=' "$pfx/user.reg" | tail -1)" = "\"Graphics\"=\"$gd\"" ] ||
  printf '\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="%s"\n' $gd >> "$pfx/user.reg"
  # the native core's files in place of the PE ones, on a linked copy
  if [ -n "$native" ]; then
    nd=$(dirname "$dist")/native-$WINE32_KEY
    [ -f "$nd/.done" ] || {
      rm -rf "$nd"; cp -rl "$dist" "$nd"
      for f in "$native"/*; do rm -f "$nd/lib/wine/x86_64-windows/${f##*/}"; cp "$f" "$nd/lib/wine/x86_64-windows/"; done
      touch "$nd/.done"; }
    dist=$nd
  fi
  loader=$dist/lib/wine/x86_64-unix/wine
  [ -d "$dist/lib/wine/i386-windows" ] && envs+=(--env WINEGEX_WOW64=1)
fi
win="Z:$(printf %s "$exe" | tr / '\\')"
gl=(); [ $graphics = wayland ] && gl=(--gl)
cd "$(dirname "$exe")"
export GEX_WINE32_UNITS=${GEX_WINE32_UNITS:-all} GEX_CGEN_CACHE=$units
[ -n "$build_units" ] && export GEX_WINE32_UNITS_BUILD=1
cmd=("$gexc" run --env WINEPREFIX="$pfx" --env WINEDEBUG="$debug" "${envs[@]}" "${gl[@]}"
  --wayland "$wl/wl" --wayland-size "$size" --wayland-out "$out" "$loader" "$win" "$@")
rc=0
if [ -n "$tmo" ]; then timeout -k 10 "$tmo" "${cmd[@]}" || rc=$?; else "${cmd[@]}" || rc=$?; fi
exit $rc
