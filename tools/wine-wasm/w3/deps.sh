#!/bin/bash
# The Unix libraries the W3 loader links in, static wasm32 builds (versions
# and sha256s in w3.pins; docs/wine-native.md §3.12, W3.3-W3.5):
#   FreeType (win32u), libwayland-client/-egl, libxml2, libxkbcommon and
#   libxkbregistry (winewayland.drv), a libffi stand-in (w3/ffi: the one
#   ffi_call libwayland makes, all-i32 arguments), gex-gl's EGL headers and
#   pkg-config file (the library itself, guest/gl/gexgl.c, is compiled by the
#   Makefile: it changes with gex), and OSSv4's sys/soundcard.h (w3/oss,
#   for wineoss.drv).
#
#   w3/deps.sh OUT DL
#
# OUT/install (include/, lib/*.a, lib/pkgconfig), OUT/host/wayland-scanner.
# DL holds the tarballs (w3/dl.sh). Env: GEX_WASM_SYSROOT, W3_JOBS, and the
# *_VERSION/*_URL/*_SHA256 pins (the Makefile exports them).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
WCC=$here/../wcc
out=$(realpath -m "$1") dl=$(realpath -m "$2")
J=${W3_JOBS:-12}
for p in FREETYPE WAYLAND LIBXML2 XKBCOMMON MESON; do
  u=${p}_URL s=${p}_SHA256; "$here/dl.sh" "${!u}" "${!s}" "$dl"
done
rm -rf "$out" && mkdir -p "$out/install/lib/pkgconfig" "$out/install/include" "$out/host" "$out/build" "$out/src"
I=$out/install S=$out/src
cd "$S"

# FreeType: no zlib/png/bzip2/harfbuzz/brotli.
tar xf "$dl/freetype-$FREETYPE_VERSION.tar.xz"
(cd freetype-$FREETYPE_VERSION &&
  ./configure --host=wasm32-unknown-linux-musl --prefix="$I" --enable-static --disable-shared \
    --without-zlib --without-bzip2 --without-png --without-harfbuzz --without-brotli \
    CC="$WCC" CFLAGS=-O2 > "$out/freetype-configure.log" 2>&1 &&
  make -j"$J" > "$out/freetype-make.log" 2>&1 && make install > "$out/freetype-install.log" 2>&1)

# wayland: the client library, wayland-egl and the host's scanner, compiled
# directly (no meson for so few files).
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
CF="-O2 -fvisibility=default -I $B -I $W/src -I $here/ffi -include $B/config.h"
objs=""
cp "$B/config.h" "$W/config.h"
for c in "$W/src/wayland-client.c" "$W/src/connection.c" "$W/src/wayland-util.c" "$W/src/wayland-os.c" "$B/wayland-protocol.c" "$here/ffi/ffi.c"; do
  o=$B/$(basename "$c" .c).o; "$WCC" $CF -c -o "$o" "$c"; objs="$objs $o"
done
llvm-ar rcs "$I/lib/libwayland-client.a" $objs
"$WCC" $CF -c -o "$B/wayland-egl.o" "$W/egl/wayland-egl.c"
llvm-ar rcs "$I/lib/libwayland-egl.a" "$B/wayland-egl.o"
cp "$W/src/wayland-client.h" "$W/src/wayland-client-core.h" "$W/src/wayland-util.h" "$B/wayland-version.h" \
   "$B/wayland-client-protocol.h" "$W/egl/wayland-egl.h" "$W/egl/wayland-egl-core.h" "$W/egl/wayland-egl-backend.h" "$I/include/"
cp "$W/protocol/wayland.xml" "$I/"
for p in wayland-client wayland-egl; do
  printf 'prefix=%s\nName: %s\nDescription: %s (wasm32, static)\nVersion: %s\nCflags: -I${prefix}/include\nLibs: ${prefix}/lib/lib%s.a\n' "$I" $p $p "$V" $p > "$I/lib/pkgconfig/$p.pc"
done

# libxml2 (xkbregistry's), autotools.
tar xf "$dl/libxml2-$LIBXML2_VERSION.tar.xz"
(cd libxml2-$LIBXML2_VERSION &&
  ./configure --host=wasm32-unknown-linux-musl --prefix="$I" --enable-static --disable-shared \
    --without-python --without-zlib --without-lzma --without-http --without-ftp --without-modules \
    --without-threads --without-catalog --without-debug --without-icu CC="$WCC" CFLAGS=-O2 > "$out/xml-configure.log" 2>&1 &&
  make -j"$J" libxml2.la > "$out/xml-make.log" 2>&1 &&
  make install-libLTLIBRARIES install-data > "$out/xml-install.log" 2>&1)

# libxkbcommon and libxkbregistry, meson from its wheel.
rm -rf "$out/meson" && mkdir -p "$out/meson" && (cd "$out/meson" && python3 -m zipfile -e "$dl/meson-$MESON_VERSION-py3-none-any.whl" .)
cat > "$out/wasm32-cross.ini" <<INI
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
cpu_family = 'wasm32'
cpu = 'wasm32'
endian = 'little'
INI
tar xf "$dl/libxkbcommon-$XKBCOMMON_VERSION.tar.xz"
PYTHONPATH=$out/meson python3 -m mesonbuild.mesonmain setup xkb-build libxkbcommon-$XKBCOMMON_VERSION \
  --cross-file "$out/wasm32-cross.ini" --prefix="$I" --default-library=static --buildtype=release \
  -Denable-x11=false -Denable-wayland=false -Denable-docs=false -Denable-tools=false -Denable-bash-completion=false \
  -Dxkb-config-root=/usr/share/X11/xkb -Dx-locale-root=/usr/share/X11/locale > "$out/xkb-setup.log" 2>&1
ninja -C xkb-build -j"$J" libxkbcommon.a libxkbregistry.a > "$out/xkb-make.log" 2>&1
PYTHONPATH=$out/meson python3 -m mesonbuild.mesonmain install -C xkb-build --no-rebuild --tags devel,runtime > "$out/xkb-install.log" 2>&1

# gex-gl's EGL: Khronos's headers (the host's) and a pkg-config file whose
# library is the gexgl.o the Makefile builds (W3_GL_O; it changes with gex,
# these libraries do not).
mkdir -p "$I/egl/include"
cp -r /usr/include/EGL /usr/include/KHR "$I/egl/include/"
printf 'Name: egl\nDescription: gex-gl EGL (wasm32, static: guest/gl/gexgl.c)\nVersion: 1.5\nCflags: -I%s\nLibs: %s\n' \
  "$I/egl/include" "${W3_GL_O:?W3_GL_O}" > "$I/lib/pkgconfig/egl.pc"

# OSSv4's header for wineoss.drv (configure finds none in the sysroot).
mkdir -p "$I/oss/sys" && cp "$here/oss/sys/soundcard.h" "$I/oss/sys/"
rm -rf "$S"
echo "w3/deps: $I"
