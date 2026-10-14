#!/bin/bash
# 12: the static wasm32 libraries the W3 loader links in, and the host's
# wayland-scanner, in $W7_OUT/deps (versions and sha256s pinned here):
#   ft/   FreeType 2.13.3 (win32u's fonts)
#   wl/   libwayland-client/-egl 1.23.1, libxml2 2.12.9, libxkbcommon and
#         libxkbregistry 1.7.0 (winewayland.drv); wl/host/wayland-scanner;
#         libffi's one use is wl-ffi/ (all-i32 ffi_call)
#   gl/   gex-gl's guest EGL (guest/gl/gexgl.c) as one object, with an egl.pc
#         and the system's EGL headers (libegl-dev)
#   oss/  FreeBSD's OSSv4 sys/soundcard.h (oss/soundcard.h here) for wineoss
# meson 1.5.2 (for libxkbcommon) is pip-installed into deps/pymeson.
. "$(dirname "$0")/lib.sh"
D=$W7_OUT/deps; mkdir -p "$D"; L=$W7_OUT/logs
[ -x "$GEX_WASM_SYSROOT/binaryen/bin/wasm-opt" ] || { echo "run 10-sysroot.sh first" >&2; exit 2; }

step "FreeType"
ft=$(fetch https://download.savannah.gnu.org/releases/freetype/freetype-2.13.3.tar.xz 0550350666d427c74daeb85d5ac7bb353acba5f76956395995311a9c6f063289)
rm -rf "$D/ft"; mkdir -p "$D/ft"; tar -C "$D/ft" -xf "$ft"
(cd "$D/ft/freetype-2.13.3" &&
  ./configure --host=wasm32-unknown-linux-musl --prefix="$D/ft/install" --enable-static --disable-shared \
    --without-zlib --without-bzip2 --without-png --without-harfbuzz --without-brotli CC="$WCC" CFLAGS=-O2 &&
  make -j"$JOBS" && make install) > "$L/deps-ft.log" 2>&1 || { tail -20 "$L/deps-ft.log"; exit 1; }

step "Wayland, libxml2, libxkbcommon"
V=1.23.1; W=$D/wl; S=$W/wayland-$V
wl=$(fetch https://gitlab.freedesktop.org/wayland/wayland/-/releases/$V/downloads/wayland-$V.tar.xz 864fb2a8399e2d0ec39d56e9d9b753c093775beadc6022ce81f441929a81e5ed)
xml=$(fetch https://download.gnome.org/sources/libxml2/2.12/libxml2-2.12.9.tar.xz 59912db536ab56a3996489ea0299768c7bcffe57169f0235e7f962a91f483590)
xkb=$(fetch https://xkbcommon.org/download/libxkbcommon-1.7.0.tar.xz 65782f0a10a4b455af9c6baab7040e2f537520caa2ec2092805cdfd36863b247)
rm -rf "$W"; mkdir -p "$W/host" "$W/build" "$W/install/lib/pkgconfig" "$W/install/include"; cd "$W"
tar -xf "$wl"; cp -r "$W7_TOOLS/wl-ffi" "$W/ffi"
{
IFS=. read -r MA MI MC <<< "$V"
sed -e "s/@WAYLAND_VERSION@/$V/;s/@WAYLAND_VERSION_MAJOR@/$MA/;s/@WAYLAND_VERSION_MINOR@/$MI/;s/@WAYLAND_VERSION_MICRO@/$MC/" \
  "$S/src/wayland-version.h.in" > build/wayland-version.h
printf '#define HAVE_STRNDUP 1\n#define HAVE_SYS_PRCTL_H 1\n#define HAVE_XUCRED_CR_PID 0\n#define HAVE_BROKEN_MSG_CMSG_CLOEXEC 0\n#define PACKAGE_VERSION "%s"\n' $V > host/config.h
printf '#define HAVE_ACCEPT4 1\n#define HAVE_MKOSTEMP 1\n#define HAVE_POSIX_FALLOCATE 1\n#define HAVE_MEMFD_CREATE 1\n#define HAVE_MREMAP 1\n#define HAVE_STRNDUP 1\n#define HAVE_XUCRED_CR_PID 0\n#define HAVE_BROKEN_MSG_CMSG_CLOEXEC 0\n#define PACKAGE_VERSION "%s"\n' $V > build/config.h
python3 "$S/src/embed.py" "$S/protocol/wayland.dtd" wayland_dtd > host/wayland.dtd.h
cc -O2 -include host/config.h -I host -I build -I "$S/src" -I "$S/protocol" -o host/wayland-scanner \
  "$S/src/scanner.c" "$S/src/wayland-util.c" -lexpat
host/wayland-scanner client-header "$S/protocol/wayland.xml" build/wayland-client-protocol.h
host/wayland-scanner public-code "$S/protocol/wayland.xml" build/wayland-protocol.c
CF="-O2 -fvisibility=default -I $W/build -I $S/src -I $W/ffi -include $W/build/config.h"
objs=""; cp build/config.h "$S/config.h"
for c in "$S/src/wayland-client.c" "$S/src/connection.c" "$S/src/wayland-util.c" "$S/src/wayland-os.c" build/wayland-protocol.c "$W/ffi/ffi.c"; do
  o=build/$(basename "$c" .c).o; "$WCC" $CF -c -o "$o" "$c"; objs="$objs $o"
done
llvm-ar rcs install/lib/libwayland-client.a $objs
"$WCC" $CF -c -o build/wayland-egl.o "$S/egl/wayland-egl.c"
llvm-ar rcs install/lib/libwayland-egl.a build/wayland-egl.o
cp "$S/src/wayland-client.h" "$S/src/wayland-client-core.h" "$S/src/wayland-util.h" build/wayland-version.h \
   build/wayland-client-protocol.h "$S/egl/wayland-egl.h" "$S/egl/wayland-egl-core.h" "$S/egl/wayland-egl-backend.h" install/include/
cp "$S/protocol/wayland.xml" install/
for p in wayland-client wayland-egl; do
  printf 'prefix=%s\nName: %s\nDescription: %s (wasm32, static)\nVersion: %s\nCflags: -I${prefix}/include\nLibs: ${prefix}/lib/lib%s.a\n' "$W/install" $p $p $V $p > install/lib/pkgconfig/$p.pc
done
tar -xf "$xml"
(cd libxml2-2.12.9 &&
  ./configure --host=wasm32-unknown-linux-musl --prefix="$W/install" --enable-static --disable-shared \
    --without-python --without-zlib --without-lzma --without-http --without-ftp --without-modules \
    --without-threads --without-catalog --without-debug --without-icu CC="$WCC" CFLAGS=-O2 &&
  make -j"$JOBS" libxml2.la && make install-libLTLIBRARIES install-data)
pip install -q --target "$D/pymeson" meson==1.5.2
cat > "$W/wasm32-cross.ini" <<INI
[binaries]
c = '$WCC'
ar = 'llvm-ar'
strip = 'llvm-strip'
pkg-config = 'pkg-config'
[properties]
needs_exe_wrapper = true
pkg_config_libdir = '$W/install/lib/pkgconfig'
[host_machine]
system = 'linux'
cpu_family = 'wasm32'
cpu = 'wasm32'
endian = 'little'
INI
tar -xf "$xkb"
PYTHONPATH=$D/pymeson python3 -m mesonbuild.mesonmain setup xkb-build libxkbcommon-1.7.0 \
  --cross-file "$W/wasm32-cross.ini" --prefix="$W/install" --default-library=static --buildtype=release \
  -Denable-x11=false -Denable-wayland=false -Denable-docs=false -Denable-tools=false -Denable-bash-completion=false \
  -Dxkb-config-root=/usr/share/X11/xkb -Dx-locale-root=/usr/share/X11/locale
ninja -C xkb-build -j"$JOBS" libxkbcommon.a libxkbregistry.a
PYTHONPATH=$D/pymeson python3 -m mesonbuild.mesonmain install -C xkb-build --no-rebuild --tags devel,runtime
} > "$L/deps-wl.log" 2>&1 || { tail -20 "$L/deps-wl.log"; exit 1; }

step "gex-gl's EGL, the OSS header"
G=$D/gl; rm -rf "$G"; mkdir -p "$G/include" "$G/pkgconfig"
"$WCC" -O2 -c -o "$G/gexgl.o" "$GEX_SRC/guest/gl/gexgl.c" > "$L/deps-gl.log" 2>&1 || { tail "$L/deps-gl.log"; exit 1; }
cp -r /usr/include/EGL /usr/include/KHR "$G/include/"
printf 'Name: egl\nDescription: gex-gl EGL (wasm32, static: guest/gl/gexgl.c)\nVersion: 1.5\nCflags: -I%s\nLibs: %s\n' "$G/include" "$G/gexgl.o" > "$G/pkgconfig/egl.pc"
mkdir -p "$D/oss/include/sys"; cp "$W7_TOOLS/oss/soundcard.h" "$D/oss/include/sys/soundcard.h"
ls "$D/ft/install/lib/libfreetype.a" "$W/install/lib/libxkbcommon.a" "$W/install/lib/libxkbregistry.a" "$W/install/lib/libxml2.a" "$W/host/wayland-scanner" > /dev/null
step "done: $D"
