#!/bin/bash
# The Unix libraries W64's loader links for a display (docs/wine-native-x64.md
# §5, W64.4): w3/deps.sh's, built LP64 for wasm64 (wcc64, lowered with the
# program) at w3.pins' versions:
#   FreeType (win32u), libwayland-client/-egl, libxml2, libxkbcommon and
#   libxkbregistry (winewayland.drv), and ffi64/, the libffi stand-in for
#   libwayland's one ffi_call (64-bit pointers: a call table by argument
#   pattern, ffi64/gen.py). No EGL and no OSS yet.
#
#   tools/wine-wasm/w64-deps.sh OUT DL
#
# OUT/install (include/, lib/*.a, lib/pkgconfig), OUT/host/wayland-scanner.
# DL holds the tarballs (w3/dl.sh fetches the missing ones, sha256-checked).
# Env: GEX_WASM64_SYSROOT, JOBS.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
WCC=$here/wcc64 HOST=wasm64-unknown-linux-musl
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
out=$(realpath -m "${1:?OUT}") dl=$(realpath -m "${2:?DL}")
J=${JOBS:-12}
eval "$(sed -n 's/^\([A-Z0-9_]*\) *:= *\(.*\)$/\1=\2/p' "$here/w3.pins")"
for p in FREETYPE WAYLAND LIBXML2 XKBCOMMON MESON; do
  u=${p}_URL s=${p}_SHA256; "$here/w3/dl.sh" "${!u}" "${!s}" "$dl"
done
rm -rf "$out" && mkdir -p "$out/install/lib/pkgconfig" "$out/install/include" "$out/host" "$out/build" "$out/src"
I=$out/install S=$out/src
cd "$S"

# FreeType: no zlib/png/bzip2/harfbuzz/brotli.
tar xf "$dl/freetype-$FREETYPE_VERSION.tar.xz"
(cd freetype-$FREETYPE_VERSION &&
  ./configure --host=$HOST --prefix="$I" --enable-static --disable-shared \
    --without-zlib --without-bzip2 --without-png --without-harfbuzz --without-brotli \
    CC="$WCC" CFLAGS=-O2 > "$out/freetype-configure.log" 2>&1 &&
  make -j"$J" > "$out/freetype-make.log" 2>&1 && make install > "$out/freetype-install.log" 2>&1)

# wayland: the client library, wayland-egl and the host's scanner, compiled
# directly (as w3/deps.sh).
V=$WAYLAND_VERSION W=$S/wayland-$V B=$out/build H=$out/host
tar xf "$dl/wayland-$V.tar.xz"
IFS=. read -r MA MI MC <<< "$V"
sed -e "s/@WAYLAND_VERSION@/$V/;s/@WAYLAND_VERSION_MAJOR@/$MA/;s/@WAYLAND_VERSION_MINOR@/$MI/;s/@WAYLAND_VERSION_MICRO@/$MC/" \
  "$W/src/wayland-version.h.in" > "$B/wayland-version.h"
printf '#define HAVE_STRNDUP 1\n#define HAVE_SYS_PRCTL_H 1\n#define HAVE_XUCRED_CR_PID 0\n#define HAVE_BROKEN_MSG_CMSG_CLOEXEC 0\n#define PACKAGE_VERSION "%s"\n' "$V" > "$H/config.h"
printf '#define HAVE_ACCEPT4 1\n#define HAVE_MKOSTEMP 1\n#define HAVE_POSIX_FALLOCATE 1\n#define HAVE_MEMFD_CREATE 1\n#define HAVE_MREMAP 1\n#define HAVE_STRNDUP 1\n#define HAVE_XUCRED_CR_PID 0\n#define HAVE_BROKEN_MSG_CMSG_CLOEXEC 0\n#define PACKAGE_VERSION "%s"\n' "$V" > "$B/config.h"
python3 "$W/src/embed.py" "$W/protocol/wayland.dtd" wayland_dtd > "$H/wayland.dtd.h"
cc -O2 -include "$H/config.h" -I "$H" -I "$B" -I "$W/src" -I "$W/protocol" -o "$H/wayland-scanner" \
  "$W/src/scanner.c" "$W/src/wayland-util.c" -lexpat
"$H/wayland-scanner" client-header "$W/protocol/wayland.xml" "$B/wayland-client-protocol.h"
"$H/wayland-scanner" public-code "$W/protocol/wayland.xml" "$B/wayland-protocol.c"
python3 "$here/ffi64/gen.py" > "$B/ffi64.inc"
CF="-O2 -fvisibility=default -I $B -I $W/src -I $here/ffi64 -include $B/config.h"
objs=""
cp "$B/config.h" "$W/config.h"
for c in "$W/src/wayland-client.c" "$W/src/connection.c" "$W/src/wayland-util.c" "$W/src/wayland-os.c" "$B/wayland-protocol.c" "$here/ffi64/ffi.c"; do
  o=$B/$(basename "$c" .c).o; "$WCC" $CF -c -o "$o" "$c"; objs="$objs $o"
done
llvm-ar rcs "$I/lib/libwayland-client.a" $objs
"$WCC" $CF -c -o "$B/wayland-egl.o" "$W/egl/wayland-egl.c"
llvm-ar rcs "$I/lib/libwayland-egl.a" "$B/wayland-egl.o"
cp "$W/src/wayland-client.h" "$W/src/wayland-client-core.h" "$W/src/wayland-util.h" "$B/wayland-version.h" \
   "$B/wayland-client-protocol.h" "$W/egl/wayland-egl.h" "$W/egl/wayland-egl-core.h" "$W/egl/wayland-egl-backend.h" "$I/include/"
cp "$W/protocol/wayland.xml" "$I/"
for p in wayland-client wayland-egl; do
  printf 'prefix=%s\nName: %s\nDescription: %s (LP64 wasm64, static)\nVersion: %s\nCflags: -I${prefix}/include\nLibs: ${prefix}/lib/lib%s.a\n' "$I" $p $p "$V" $p > "$I/lib/pkgconfig/$p.pc"
done

# libxml2 (xkbregistry's), autotools.
tar xf "$dl/libxml2-$LIBXML2_VERSION.tar.xz"
(cd libxml2-$LIBXML2_VERSION &&
  ./configure --host=$HOST --prefix="$I" --enable-static --disable-shared \
    --without-python --without-zlib --without-lzma --without-http --without-ftp --without-modules \
    --without-threads --without-catalog --without-debug --without-icu CC="$WCC" CFLAGS=-O2 > "$out/xml-configure.log" 2>&1 &&
  make -j"$J" libxml2.la > "$out/xml-make.log" 2>&1 &&
  make install-libLTLIBRARIES install-data > "$out/xml-install.log" 2>&1)

# libxkbcommon and libxkbregistry, meson from its wheel.
rm -rf "$out/meson" && mkdir -p "$out/meson" && (cd "$out/meson" && python3 -m zipfile -e "$dl/meson-$MESON_VERSION-py3-none-any.whl" .)
cat > "$out/wasm64-cross.ini" <<INI
[binaries]
c = '$WCC'
ar = 'llvm-ar'
strip = 'llvm-strip'
pkg-config = 'pkg-config'
[properties]
needs_exe_wrapper = true
pkg_config_libdir = '$I/lib/pkgconfig'
[host_machine]
system = 'linux'
cpu_family = 'wasm64'
cpu = 'wasm64'
endian = 'little'
INI
tar xf "$dl/libxkbcommon-$XKBCOMMON_VERSION.tar.xz"
PYTHONPATH=$out/meson python3 -m mesonbuild.mesonmain setup xkb-build libxkbcommon-$XKBCOMMON_VERSION \
  --cross-file "$out/wasm64-cross.ini" --prefix="$I" --default-library=static --buildtype=release \
  -Denable-x11=false -Denable-wayland=false -Denable-docs=false -Denable-tools=false -Denable-bash-completion=false \
  -Dxkb-config-root=/usr/share/X11/xkb -Dx-locale-root=/usr/share/X11/locale > "$out/xkb-setup.log" 2>&1
ninja -C xkb-build -j"$J" libxkbcommon.a libxkbregistry.a > "$out/xkb-make.log" 2>&1
PYTHONPATH=$out/meson python3 -m mesonbuild.mesonmain install -C xkb-build --no-rebuild --tags devel,runtime > "$out/xkb-install.log" 2>&1
rm -rf "$S"
echo "w64-deps: $I"
