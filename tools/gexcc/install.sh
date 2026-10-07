#!/bin/sh
# Install the gexcc toolchain (docs/gexcc.md) into PREFIX:
#
#   tools/gexcc/install.sh PREFIX [MUSL_SRC]
#
#   PREFIX/bin/            gexcc, gexc++, wasm32-unknown-linux-musl-* tools
#   PREFIX/lib/gexcc/      the driver's helpers and the container's host stub
#   PREFIX/sysroot/        musl (gex's wasm32 arch), libgex, compiler-rt,
#                          binaryen; usr/ for cross-built libraries
#   PREFIX/share/gexcc/    toolchain.cmake, meson-cross.ini, config.sub
#
# MUSL_SRC: a checkout of the musl fork (MercuryWorkshop/musl-wasm, branch
# gex-wasm32-slim or a descendant). GEXC: the gexc a program's host launcher
# runs (default: this tree's target/release/gexc if built, else `gexc` on
# PATH at run time). GEX_BINARYEN: an existing binaryen install.
# BUSYBOX_TAR: busybox 1.37.0's tarball, built as the guest's native
# /bin/sh and coreutils. LLVM_SRC: an LLVM tree for libc++ (build-cxx.sh).
set -eu
PREFIX=$(realpath -m "$1")
HERE=$(dirname "$(realpath "$0")")
ROOT=$(realpath "$HERE/../..")
WW=$ROOT/tools/wine-wasm
MUSL=$(realpath "${2:-$ROOT/../musl-wasm}")
TRIPLE=wasm32-unknown-linux-musl
mkdir -p "$PREFIX/bin" "$PREFIX/lib/gexcc" "$PREFIX/share/gexcc" "$PREFIX/build"

# the sysroot (musl, libgex, compiler-rt, UAPI headers, binaryen)
GEX_WASM_LEGACY_EH=1 "$WW/build-sysroot.sh" "$PREFIX/build" "$MUSL"
ln -sfn build/sysroot "$PREFIX/sysroot"
mkdir -p "$PREFIX/sysroot/usr/include" "$PREFIX/sysroot/usr/lib/pkgconfig" "$PREFIX/sysroot/usr/share/pkgconfig"
# the rest of the Linux UAPI headers (build-sysroot.sh copies linux/, asm/,
# asm-generic/): architecture-neutral, from the host's linux-libc-dev
for d in mtd sound video xen misc rdma; do
  [ -d "/usr/include/$d" ] && { rm -rf "$PREFIX/sysroot/include/$d"; cp -r "/usr/include/$d" "$PREFIX/sysroot/include/"; }
done

# gexcc's wrapper headers (include/: setjmp.h), searched before the libc's
rm -rf "$PREFIX/sysroot/include/gexcc"
cp -r "$HERE/include" "$PREFIX/sysroot/include/gexcc"

# the driver and its helpers
L=$PREFIX/lib/gexcc
cp "$HERE/gexcc" "$PREFIX/bin/gexcc"
cp "$HERE/elemfix.py" "$PREFIX/lib/gexcc/"
cp "$WW/flags.sh" "$WW/wasm-elf.py" "$WW/forkreach.py" "$L/"
# the container's host launcher: flat i386 code
clang --target=i386-linux-gnu -Os -fPIE -ffreestanding -fno-stack-protector -fno-asynchronous-unwind-tables \
  -fno-builtin -c "$HERE/hoststub.c" -o "$L/hoststub.o"
ld.lld -m elf_i386 -pie --no-dynamic-linker -T "$HERE/hoststub.ld" -o "$L/hoststub.elf" "$L/hoststub.o"
llvm-objcopy -O binary "$L/hoststub.elf" "$L/hoststub.bin"
rm -f "$L/hoststub.o" "$L/hoststub.elf"
# gexcc's runtime (rt/: the loop safepoint) and the GexPoll pass plugin
"$HERE/poll/build.sh" "$L"
mkdir -p "$PREFIX/build/rt"
. "$WW/flags.sh"
clang $WASM_CFLAGS --sysroot="$PREFIX/sysroot" -c "$HERE/rt/main3.c" -o "$L/main3.o"
for f in "$HERE"/rt/poll.c "$HERE"/rt/sigsetjmp.c "$HERE"/rt/*.s; do
  clang $WASM_CFLAGS --sysroot="$PREFIX/sysroot" -c "$f" -o "$PREFIX/build/rt/$(basename "$f").o"
done
rm -f "$PREFIX/sysroot/lib/libgexcc.a"
llvm-ar crs "$PREFIX/sysroot/lib/libgexcc.a" "$PREFIX"/build/rt/*.o
# an empty object: -shared's archives carry their deps in a copy of it
echo 'typedef int gexcc_empty;' | clang $WASM_CFLAGS -x c -c - -o "$L/empty.o"
GEXC=${GEXC:-}
[ -z "$GEXC" ] && [ -x "$ROOT/target/release/gexc" ] && GEXC=$ROOT/target/release/gexc
G=gexc
[ -n "$GEXC" ] && G=$(realpath "$GEXC")
# Containers run under gexcc-exec (below), which adds gexcc's defaults to gexc.
echo "$PREFIX/bin/gexcc-exec" > "$L/runner"

# tool names autotools (--host=$TRIPLE), CMake and Meson look for
B=$PREFIX/bin
ln -sf gexcc "$B/gexc++"
for t in cc gcc clang; do ln -sf gexcc "$B/$TRIPLE-$t"; done
for t in c++ g++ clang++; do ln -sf gexcc "$B/$TRIPLE-$t"; done
# (the driver picks C++ by its name ending in ++)
for t in ar ranlib nm objdump strings size readelf objcopy; do
  printf '#!/bin/sh\nexec llvm-%s "$@"\n' "$t" > "$B/$TRIPLE-$t"; chmod +x "$B/$TRIPLE-$t"
done
for t in gexcc-ar gexcc-ranlib gexcc-nm; do
  printf '#!/bin/sh\nexec llvm-%s "$@"\n' "${t#gexcc-}" > "$B/$t"; chmod +x "$B/$t"
done
# ld: wasm-ld (libtool asks for it; relocatable links, `ld -r`, mostly).
# Its version says it is GNU-compatible, as ld.lld's does: libtool decides
# from that whether it knows how to make shared libraries.
cat > "$B/$TRIPLE-ld" <<'LDEOF'
#!/bin/sh
case "$1" in
  -v|-V|--version) echo "$(wasm-ld --version | head -1) (compatible with GNU linkers)"; exit 0 ;;
esac
exec wasm-ld "$@"
LDEOF
chmod +x "$B/$TRIPLE-ld"
# strip: a program is a container around a wasm module; nothing to strip
# (the module's names are what gex binds by)
cat > "$B/$TRIPLE-strip" <<'EOF'
#!/bin/sh
# gexcc: programs and archives are left as they are
exit 0
EOF
chmod +x "$B/$TRIPLE-strip"
ln -sf "$TRIPLE-strip" "$B/gexcc-strip"
cat > "$B/$TRIPLE-pkg-config" <<EOF
#!/bin/sh
# pkg-config over the gexcc sysroot's libraries only
export PKG_CONFIG_SYSROOT_DIR="$PREFIX/sysroot"
export PKG_CONFIG_LIBDIR="$PREFIX/sysroot/usr/lib/pkgconfig:$PREFIX/sysroot/usr/share/pkgconfig:$PREFIX/sysroot/usr/local/lib/pkgconfig"
unset PKG_CONFIG_PATH
exec pkg-config --static "\$@"
EOF
chmod +x "$B/$TRIPLE-pkg-config"
ln -sf "$TRIPLE-pkg-config" "$B/gexcc-pkg-config"
# gexcc-exec: gexc for gexcc programs (the containers' host launcher runs
# it). The native busybox is the guest's /bin/sh and coreutils, unless
# GEXCC_BUSYBOX is set (empty: none; a path: that busybox). GEXCC_GEXC
# picks another gexc.
cat > "$B/gexcc-exec" <<EOF
#!/bin/sh
[ "\$1" = run ] && shift
bb=\${GEXCC_BUSYBOX-$L/busybox}
if [ -n "\$bb" ] && [ -f "\$bb" ]; then
  if [ "\$bb" = "$L/busybox" ] && [ -z "\${GEX_BUSYBOX_APPLETS:-}" ] && [ -f "$L/busybox.applets" ]; then
    GEX_BUSYBOX_APPLETS=\$(cat "$L/busybox.applets"); export GEX_BUSYBOX_APPLETS
  fi
  exec "\${GEXCC_GEXC:-$G}" run --busybox "\$bb" "\$@"
fi
exec "\${GEXCC_GEXC:-$G}" run "\$@"
EOF
chmod +x "$B/gexcc-exec"
# `gexcc-run PROG ARGS`: what the container's launcher does, by hand
R=$B/gexcc-exec
cat > "$B/gexcc-run" <<EOF
#!/bin/sh
exec "\${GEXCC_RUNNER:-$R}" run --host-env "\$@"
EOF
chmod +x "$B/gexcc-run"

# build-system files
S=$PREFIX/share/gexcc
cp /usr/share/misc/config.sub /usr/share/misc/config.guess "$S/" 2>/dev/null || true
cat > "$S/toolchain.cmake" <<EOF
# CMake toolchain file for gexcc: cmake -DCMAKE_TOOLCHAIN_FILE=$S/toolchain.cmake
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR wasm32)
set(CMAKE_C_COMPILER "$B/gexcc")
set(CMAKE_CXX_COMPILER "$B/gexc++")
set(CMAKE_AR "$B/$TRIPLE-ar" CACHE FILEPATH "")
set(CMAKE_RANLIB "$B/$TRIPLE-ranlib" CACHE FILEPATH "")
set(CMAKE_NM "$B/$TRIPLE-nm" CACHE FILEPATH "")
set(CMAKE_STRIP "$B/$TRIPLE-strip" CACHE FILEPATH "")
set(CMAKE_SYSROOT "$PREFIX/sysroot")
set(CMAKE_STAGING_PREFIX "$PREFIX/sysroot/usr" CACHE PATH "")
set(CMAKE_FIND_ROOT_PATH "$PREFIX/sysroot")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
set(CMAKE_CROSSCOMPILING_EMULATOR "$B/gexcc-run")
set(PKG_CONFIG_EXECUTABLE "$B/$TRIPLE-pkg-config" CACHE FILEPATH "")
set(BUILD_SHARED_LIBS OFF CACHE BOOL "")
EOF
cat > "$S/meson-cross.ini" <<EOF
# Meson cross file for gexcc: meson setup --cross-file $S/meson-cross.ini
[binaries]
c = '$B/gexcc'
cpp = '$B/gexc++'
ar = '$B/$TRIPLE-ar'
ranlib = '$B/$TRIPLE-ranlib'
nm = '$B/$TRIPLE-nm'
strip = '$B/$TRIPLE-strip'
pkg-config = '$B/$TRIPLE-pkg-config'
pkgconfig = '$B/$TRIPLE-pkg-config'
exe_wrapper = '$B/gexcc-run'

[built-in options]
default_library = 'static'

[properties]
sys_root = '$PREFIX/sysroot'
needs_exe_wrapper = false

[host_machine]
system = 'linux'
cpu_family = 'wasm32'
cpu = 'wasm32'
endian = 'little'
EOF

# busybox (BUSYBOX_TAR: its 1.37.0 tarball), built with the driver just
# installed: the native /bin/sh and coreutils gexcc programs see inside gex
# (system(), popen, exec of sh, sleep, kill). A translated host shell takes
# seconds per command.
if [ -n "${BUSYBOX_TAR:-}" ]; then
  BB=$PREFIX/build/busybox
  rm -rf "$BB"; mkdir -p "$BB"
  tar xjf "$BUSYBOX_TAR" -C "$BB" --strip-components=1
  (cd "$BB" && make defconfig > /dev/null 2>&1 &&
   sed -i -e 's/^CONFIG_SHA1_HWACCEL=y/# CONFIG_SHA1_HWACCEL is not set/' \
          -e 's/^CONFIG_SHA256_HWACCEL=y/# CONFIG_SHA256_HWACCEL is not set/' \
          -e 's/^CONFIG_TC=y/# CONFIG_TC is not set/' .config &&
   PATH="$B:$PATH" make -j"${JOBS:-6}" CC=gexcc HOSTCC=gcc AR=llvm-ar STRIP=true SKIP_STRIP=y > build.log 2>&1) ||
    { echo "busybox failed: $BB/build.log"; exit 1; }
  cp "$BB/busybox" "$L/busybox"
  GEXCC_BUSYBOX= "$B/gexcc-exec" run "$L/busybox" --list | tr '\n' ',' | sed 's/,$//' > "$L/busybox.applets"
fi
echo "gexcc: $B/gexcc (sysroot $PREFIX/sysroot, runner $R)"
# C++ (LLVM_SRC: an LLVM source tree of clang's version)
if [ -n "${LLVM_SRC:-}" ]; then
  "$HERE/build-cxx.sh" "$PREFIX" "$LLVM_SRC"
fi
