#!/bin/bash
# Wine's build trees for 32-bit Wine:
#
#   wine32/wine-build.sh tools SRC TOOLS
#       the host tools (winebuild, widl, wrc, winegcc, ...) the cross build
#       runs: a native configure of SRC and `make __tooldeps__`.
#   wine32/wine-build.sh wasm SRC TOOLS BUILD DEPS
#       the wasm32 tree: configure SRC for wasm32-unknown-linux-musl (wcc,
#       the sysroot) with FreeType, winewayland.drv's libraries and gex-gl's
#       EGL from DEPS (wine32/deps.sh), then make. The loader and wineserver are
#       linked from its objects afterwards (wine32/link-loader.sh,
#       link-wineserver.sh).
#   wine/build32.sh configure SRC TOOLS BUILD DEPS
#       that tree configured only, not made: the compile commands the
#       wineserver is built with (dist.sh, link-wineserver.sh).
#
# Both are incremental: configure only when there is no Makefile or the
# options changed; make always (cheap when nothing changed).
# Env: GEX_WASM_SYSROOT, WINE32_JOBS.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
J=${WINE32_JOBS:-12}
# The options both trees share: what 32-bit Wine does without (no X11, no PE
# cross-compiler: the PE side is wasm32 skeletons and the host's i386 DLLs).
# inotify stays: the server watches directories with it for
# FindFirstChangeNotification and ReadDirectoryChangesW (the gex kernel has it).
OPTS="--disable-tests --without-mingw --enable-archs= --without-x --without-gstreamer --without-pulse --without-alsa
 --without-cups --without-sane --without-gphoto --without-v4l2 --without-vulkan --without-dbus --without-krb5
 --without-gssapi --without-netapi --without-pcap --without-usb --without-unwind --without-sdl --without-oss --without-udev
 --without-capi"
configure() { # DIR STAMP CMD...
  local dir=$1 stamp=$2; shift 2
  mkdir -p "$dir"
  if [ ! -f "$dir/Makefile" ] || [ "$(cat "$dir/.wine32-configure" 2>/dev/null)" != "$stamp" ]; then
    (cd "$dir" && "$@" > configure.log 2>&1) || { echo "wine32/wine-build: configure failed in $dir (tail: $(tail -3 "$dir/configure.log" | tr '\n' ' '))" >&2; exit 1; }
    echo "$stamp" > "$dir/.wine32-configure"
  fi
}
case $1 in
tools)
  src=$(realpath "$2") tools=$(realpath -m "$3")
  o="$OPTS --without-freetype --without-wayland --without-opengl"
  configure "$tools" "$src $o" "$src/configure" $o
  make -C "$tools" -j"$J" __tooldeps__ > "$tools/make.log" 2>&1 || { echo "wine32/wine-build: tools make failed ($tools/make.log)" >&2; exit 1; }
  ;;
wasm|configure)
  src=$(realpath "$2") tools=$(realpath "$3") build=$(realpath -m "$4") deps=$(realpath "$5")
  : "${GEX_WASM_SYSROOT:?}"
  # dpnsvr and twain_32: i386 builtins configure enables for i386 alone
  # (wasm32 serves i386 programs); dmsynth and its FluidSynth, which it
  # enables only with a C++ compiler: native wasm32 modules too. (CXX,
  # wcc++: FluidSynth's interpolators are C++ templates, no C++ runtime.)
  o="$OPTS --with-gnutls --without-ffmpeg --without-fontconfig --enable-dpnsvr --enable-twain_32 --enable-dmsynth --enable-fluidsynth"
  export PKG_CONFIG_LIBDIR=$deps/install/lib/pkgconfig
  # wcc: the Wine trees' paths as /wine, not this host's.
  export GEX_WCC_MAP="$src=/wine:$build=/wine-build:$tools=/wine-tools:$deps=/wine-deps"
  configure "$build" "$src $o $deps $GEX_WASM_SYSROOT" "$src/configure" --host=wasm32-unknown-linux-musl --with-wine-tools="$tools" $o \
    CC="$here/wcc" CXX="$here/wcc++" PKG_CONFIG=pkg-config WAYLAND_SCANNER="$deps/host/wayland-scanner" \
    ac_cv_lib_soname_xkbregistry=libxkbregistry.so.0 ac_cv_lib_soname_freetype=libfreetype.so.6 ac_cv_lib_soname_EGL=libEGL.so.1 \
    ac_cv_lib_soname_gnutls=libgnutls.so.30
  # configure: the tree only, for its compile commands (link-wineserver.sh)
  [ "$1" = configure ] && exit 0
  # wrc and wmc read locale.nls from the tools tree, which __tooldeps__ does
  # not make: the source's.
  # A copy, not a link: the tools tree outlives the source tree it was made
  # from (keyed by its inputs, it serves later commits and other worktrees).
  mkdir -p "$tools/nls" && { cmp -s "$src/nls/locale.nls" "$tools/nls/locale.nls" ||
    { rm -f "$tools/nls/locale.nls" && cp "$src/nls/locale.nls" "$tools/nls/locale.nls"; }; }
  # -k: a few targets have no wasm32 meaning (they fail and are not used).
  TMPDIR=${TMPDIR:-/tmp} make -C "$build" -k -j"$J" > "$build/make.log" 2>&1 || true
  # What the dist needs: the loader's and the Unix libraries' objects, and
  # every PE module's relocatable object and export list (stage-native.sh).
  for f in loader/main.o dlls/ntdll/unix/loader.o dlls/ntdll/ntdll.dll.so dlls/win32u/win32u.dll.so dlls/kernelbase/kernelbase.dll.so; do
    [ -e "$build/$f" ] || { echo "wine32/wine-build: $build/$f was not built ($build/make.log)" >&2; exit 1; }
  done
  ;;
*) echo "usage: wine/build32.sh tools|wasm|configure ..." >&2; exit 2 ;;
esac
