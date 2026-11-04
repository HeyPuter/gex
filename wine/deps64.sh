#!/bin/bash
# The Unix libraries 64-bit Wine's loader links for a display:
# wine32/deps.sh's, built LP64 for wasm64 (wcc64, lowered with the
# program) at libs.pins' versions:
#   FreeType (win32u), libwayland-client/-egl, libxml2, libxkbcommon and
#   libxkbregistry (winewayland.drv), and ffi64/, the libffi stand-in for
#   libwayland's one ffi_call (64-bit pointers: a call table by argument
#   pattern, ffi64/gen.py), and gex-gl's EGL headers and pkg-config file
#   (opengl32.so; the library, guest/gl/gexgl.c, is compiled by
#   link-wine64.sh, since it changes with gex), and GnuTLS over nettle
#   (crypt32's and secur32's schannel, through dlopen of libgnutls.so.30)
#   with Mozilla's CA bundle (OUT/install/ca-certificates.crt, staged into
#   the dist), as 32-bit Wine's, and FFmpeg's libavcodec, libavformat,
#   libswscale and libswresample (winegstreamer's Unix side: its wg_*_ffmpeg.c
#   files), and SDL2 (winebus's game controllers: bus_sdl.c, through
#   dlopen of libSDL2-2.0.so.0). No OSS yet.
#
#   wine/deps64.sh OUT DL
#
# OUT/install (include/, lib/*.a, lib/pkgconfig), OUT/host/wayland-scanner.
# DL holds the tarballs (wine32/dl.sh fetches the missing ones, sha256-checked).
# Env: GEX_WASM64_SYSROOT, JOBS.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
WCC=$here/wcc64 HOST=wasm64-unknown-linux-musl
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (toolchain/sysroot/build64.sh)}"
out=$(realpath -m "${1:?OUT}") dl=$(realpath -m "${2:?DL}")
# wcc64 names the libraries' sources by OUT's place, /wine-deps, not this host's.
export GEX_WCC_MAP="${GEX_WCC_MAP:+$GEX_WCC_MAP:}$out=/wine-deps"
J=${JOBS:-12}
eval "$(sed -n 's/^\([A-Z0-9_]*\) *:= *\(.*\)$/\1=\2/p' "$here/libs.pins")"
for p in FREETYPE WAYLAND LIBXML2 XKBCOMMON MESON NETTLE GNUTLS CACERT FFMPEG SDL2; do
  u=${p}_URL s=${p}_SHA256; "$here/dl.sh" "${!u}" "${!s}" "$dl"
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
# directly (as wine32/deps.sh).
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
  -Dxkb-config-root=/usr/share/X11/xkb -Dxkb-config-extra-path=/etc/xkb -Dx-locale-root=/usr/share/X11/locale > "$out/xkb-setup.log" 2>&1
ninja -C xkb-build -j"$J" libxkbcommon.a libxkbregistry.a > "$out/xkb-make.log" 2>&1
PYTHONPATH=$out/meson python3 -m mesonbuild.mesonmain install -C xkb-build --no-rebuild --tags devel,runtime > "$out/xkb-install.log" 2>&1

# gex-gl's EGL: Khronos's headers (the host's) and a pkg-config file with no
# library (configure is given the soname; link-wine64.sh links gexgl.c).
mkdir -p "$I/egl/include"
cp -r /usr/include/EGL /usr/include/KHR "$I/egl/include/"
printf 'Name: egl\nDescription: gex-gl EGL (wasm64 LP64, linked by link-wine64.sh: guest/gl/gexgl.c)\nVersion: 1.5\nCflags: -I%s\nLibs:\n' \
  "$I/egl/include" > "$I/lib/pkgconfig/egl.pc"

# nettle with mini-gmp, then GnuTLS over it with its included libtasn1 and
# libunistring, configured as 32-bit Wine's wine32/deps.sh (no p11-kit, idn, TPM,
# compression or tools; the trust store is the path crypt32 reads).
tar xf "$dl/nettle-$NETTLE_VERSION.tar.gz"
(cd nettle-$NETTLE_VERSION &&
  ./configure --host=$HOST --prefix="$I" --libdir="$I/lib" --enable-static --disable-shared \
    --enable-mini-gmp --disable-assembler --disable-fat --disable-documentation --disable-openssl \
    CC="$WCC" CFLAGS=-O2 > "$out/nettle-configure.log" 2>&1 &&
  make -j"$J" > "$out/nettle-make.log" 2>&1 && make install > "$out/nettle-install.log" 2>&1)
tar xf "$dl/gnutls-$GNUTLS_VERSION.tar.xz"
(cd gnutls-$GNUTLS_VERSION &&
  PKG_CONFIG_LIBDIR=$I/lib/pkgconfig ./configure --host=$HOST --prefix="$I" --libdir="$I/lib" --enable-static --disable-shared \
    --with-nettle-mini --with-included-libtasn1 --with-included-unistring --without-p11-kit --without-idn \
    --without-tpm --without-tpm2 --without-trousers-lib --without-brotli --without-zstd --without-zlib --without-leancrypto \
    --disable-doc --disable-tools --disable-tests --disable-cxx --disable-nls --disable-hardware-acceleration \
    --disable-padlock --disable-libdane --disable-guile --disable-ktls \
    --with-default-trust-store-file=/etc/ssl/certs/ca-certificates.crt \
    CC="$WCC" CFLAGS=-O2 > "$out/gnutls-configure.log" 2>&1 &&
  make -j"$J" > "$out/gnutls-make.log" 2>&1 && make install > "$out/gnutls-install.log" 2>&1)
[ -f "$I/lib/libgnutls.a" ] || { echo "wine64-deps: GnuTLS failed ($out/gnutls-*.log)" >&2; exit 1; }
# FFmpeg, LGPL only (no --enable-gpl or --enable-nonfree): the decoders,
# demuxers and parsers of what Media Foundation, DirectShow and the WM reader
# name (winegstreamer's wg_format types and the containers its parser opens),
# no encoders, muxers, filters, devices, network or programs. Plain C (no
# assembly), one thread per decoder unless winegstreamer asks for more.
FF_DECODERS="h264 vc1 wmv1 wmv2 wmv3 mpeg1video mpeg2video mpeg4 msmpeg4v1 msmpeg4v2 msmpeg4v3 cinepak indeo3 indeo4
  indeo5 msvideo1 msrle mjpeg rawvideo vp8 vp9 theora
  aac aac_latm mp1 mp1float mp2 mp2float mp3 mp3float wmav1 wmav2 wmapro wmalossless wmavoice vorbis opus flac ac3 eac3
  adpcm_ms adpcm_ima_wav gsm_ms pcm_s16le pcm_s16be pcm_u8 pcm_s24le pcm_s32le pcm_f32le pcm_f64le pcm_alaw pcm_mulaw"
FF_DEMUXERS="mov asf avi mpegps mpegts mpegvideo mp3 aac wav ogg matroska flac h264 ac3"
FF_PARSERS="h264 aac aac_latm mpegaudio mpegvideo mpeg4video vc1 vp8 vp9 opus vorbis flac ac3 mjpeg"
tar xf "$dl/ffmpeg-$FFMPEG_VERSION.tar.xz"
(cd ffmpeg-$FFMPEG_VERSION &&
  ./configure --prefix="$I" --enable-cross-compile --target-os=linux --arch=wasm64 --cc="$WCC" \
    --ar=llvm-ar --ranlib=llvm-ranlib --nm=llvm-nm --strip=llvm-strip --pkg-config=false \
    --disable-everything --disable-autodetect --disable-programs --disable-doc --disable-network \
    --disable-asm --disable-inline-asm --disable-x86asm --disable-runtime-cpudetect --disable-debug \
    --disable-avdevice --disable-avfilter --enable-avcodec --enable-avformat --enable-avutil \
    --enable-swresample --enable-swscale --enable-pthreads --enable-static --disable-shared \
    $(for d in $FF_DECODERS; do printf ' --enable-decoder=%s' "$d"; done) \
    $(for d in $FF_DEMUXERS; do printf ' --enable-demuxer=%s' "$d"; done) \
    $(for d in $FF_PARSERS; do printf ' --enable-parser=%s' "$d"; done) \
    --extra-cflags=-O2 > "$out/ffmpeg-configure.log" 2>&1 &&
  make -j"$J" > "$out/ffmpeg-make.log" 2>&1 && make install > "$out/ffmpeg-install.log" 2>&1)
[ -f "$I/lib/libavcodec.a" ] || { echo "wine64-deps: FFmpeg failed ($out/ffmpeg-*.log)" >&2; exit 1; }
grep -q '^License: LGPL' "$out/ffmpeg-configure.log" || { echo "wine64-deps: FFmpeg is not LGPL ($out/ffmpeg-configure.log)" >&2; exit 1; }
# SDL2: what winebus calls (joysticks, game controllers, haptics, events,
# timers and threads), over evdev with no udev (gex-kernel's /dev/input,
# which it scans); no video (nor its dummy drivers), audio, HIDAPI or
# dynamic loading.
tar xf "$dl/SDL2-$SDL2_VERSION.tar.gz"
(cd SDL2-$SDL2_VERSION &&
  ./configure --host=$HOST --prefix="$I" --enable-static --disable-shared \
    --disable-audio --disable-video --disable-video-dummy --disable-video-offscreen --disable-render \
    --disable-sensor --disable-power --disable-filesystem --disable-file --disable-misc --disable-locale \
    --disable-hidapi --disable-hidapi-joystick --disable-hidapi-libusb --disable-joystick-virtual \
    --disable-libudev --disable-dbus --disable-ime --disable-ibus --disable-fcitx --disable-assembly \
    --disable-cpuinfo --disable-loadso --disable-rpath --disable-sdl2-config --disable-system-iconv --disable-libiconv \
    --enable-joystick --enable-haptic --enable-events --enable-timers --enable-pthreads --enable-atomic \
    CC="$WCC" CFLAGS=-O2 > "$out/sdl2-configure.log" 2>&1 &&
  make -j"$J" > "$out/sdl2-make.log" 2>&1 && make install > "$out/sdl2-install.log" 2>&1)
[ -f "$I/lib/libSDL2.a" ] || { echo "wine64-deps: SDL2 failed ($out/sdl2-*.log)" >&2; exit 1; }
cp "$dl/$(basename "$CACERT_URL")" "$I/ca-certificates.crt"
rm -rf "$S"
echo "wine64-deps: $I"
