#!/bin/sh
# Install the gexcc toolchain into PREFIX:
#
#   toolchain/gexcc/install.sh PREFIX [MUSL_SRC]
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
# PATH at run time). GEXC_X86: an x86 gexc for -fgex-guest-i386's runtime
# (default GEXC). GEX_BINARYEN: an existing binaryen install.
# BUSYBOX_TAR: busybox 1.37.0's tarball, built as the guest's native
# /bin/sh and coreutils. LLVM_SRC: a checkout of toolchain/pins' LLVM_COMMIT, for libc++ (build-cxx.sh).
# GEXCC_CACHE: a directory that keeps the sysroot, libc.so, libc++, busybox and the
# GexPoll plugin by their
# inputs between installs (below).
set -eu
PREFIX=$(realpath -m "$1")
HERE=$(dirname "$(realpath "$0")")
ROOT=$(realpath "$HERE/../..")
SR=$ROOT/toolchain/sysroot
MUSL=$(realpath "${2:-$ROOT/../musl-wasm}")
TRIPLE=wasm32-unknown-linux-musl
# gex's loader fills musl's registry of shared libraries (__gex_dsos): a
# musl without it builds, and then no shared program starts
if [ ! -e "$MUSL/src/env/wasm32/__gex_dso.c" ]; then
  echo "install.sh: $MUSL has no src/env/wasm32/__gex_dso.c (the shared-library registry gex's loader fills): use gex-wasm32-slim at or after the commit that adds it" >&2
  exit 1
fi
mkdir -p "$PREFIX/bin" "$PREFIX/lib/gexcc" "$PREFIX/share/gexcc" "$PREFIX/build"

# the sysroot (musl, libgex, compiler-rt, UAPI headers, binaryen)
# (compiler-rt's sources by their own tree's names: assert()s in the builtins
# name their files)
CRT=${GEX_CRT_SRC:-/usr/lib/emsdk/upstream/emscripten/system/lib/compiler-rt/lib/builtins}
SX="-fvisibility=default -ffile-prefix-map=$CRT=/compiler-rt/lib/builtins"
build_sysroot() { GEX_WASM_EXTRA_CFLAGS="$SX" GEX_WASM_LEGACY_EH=1 "$SR/build32.sh" "$1" "$MUSL"; }
# GEXCC_CACHE=DIR: the sysroot, libc.so, libc++, busybox and GexPoll are kept there under a key
# of everything they are built from (the files' contents, not their times),
# so a rerun with nothing changed copies them (make gexcc sets it).
CK=${GEXCC_CACHE:-}
treesum() { for p; do [ ! -e "$p" ] || find "$p" -name .git -prune -o -type f -print0 | LC_ALL=C sort -z | xargs -0 cat; done; }
hash16() { { cat; clang --version; } | sha256sum | cut -c1-16; }
if [ -n "$CK" ]; then
  SK=$( { echo "$SX ${GEX_BINARYEN:-}"; treesum "$MUSL" "$CRT" "$SR/build32.sh" "$SR/flags.sh" \
    "$SR/libgex" "$SR/libgex-snap" "$SR/icall"; } | hash16)
  if [ ! -f "$CK/sysroot-$SK/.done" ]; then
    rm -rf "$CK/sysroot-$SK.tmp" && mkdir -p "$CK"
    build_sysroot "$CK/sysroot-$SK.tmp"
    touch "$CK/sysroot-$SK.tmp/.done" && rm -rf "$CK/sysroot-$SK" && mv "$CK/sysroot-$SK.tmp" "$CK/sysroot-$SK"
  fi
  rm -rf "$PREFIX/build/sysroot" && cp -a "$CK/sysroot-$SK/sysroot" "$PREFIX/build/sysroot"
  echo "sysroot: $PREFIX/build/sysroot (cached: $CK/sysroot-$SK)"
else
  build_sysroot "$PREFIX/build"
fi
ln -sfn build/sysroot "$PREFIX/sysroot"
mkdir -p "$PREFIX/sysroot/usr/include" "$PREFIX/sysroot/usr/lib/pkgconfig" "$PREFIX/sysroot/usr/share/pkgconfig"
# the rest of the Linux UAPI headers (build32.sh copies linux/, asm/,
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
cp "$SR/flags.sh" "$HERE/wasm-elf.py" "$HERE/forkreach.py" "$L/"
# the container's host launcher: flat i386 code
clang --target=i386-linux-gnu -Os -fPIE -ffreestanding -fno-stack-protector -fno-asynchronous-unwind-tables \
  -fno-builtin -c "$HERE/hoststub.c" -o "$L/hoststub.o"
ld.lld -m elf_i386 -pie --no-dynamic-linker -T "$HERE/hoststub.ld" -o "$L/hoststub.elf" "$L/hoststub.o"
llvm-objcopy -O binary "$L/hoststub.elf" "$L/hoststub.bin"
rm -f "$L/hoststub.o" "$L/hoststub.elf"
# gexcc's runtime (rt/: the loop safepoint) and the GexPoll pass plugin
PK=
[ -z "$CK" ] || PK=$( { treesum "$HERE/poll"; ${LLVM_CONFIG:-llvm-config} --version 2>/dev/null; } | hash16)
if [ -n "$PK" ] && [ -f "$CK/poll-$PK/GexPoll.so" ]; then
  cp "$CK/poll-$PK/GexPoll.so" "$L/"
else
  "$HERE/poll/build.sh" "$L"
  [ -z "$PK" ] || { mkdir -p "$CK/poll-$PK" && cp "$L/GexPoll.so" "$CK/poll-$PK/GexPoll.so.tmp" && mv "$CK/poll-$PK/GexPoll.so.tmp" "$CK/poll-$PK/GexPoll.so"; }
fi
mkdir -p "$PREFIX/build/rt"
. "$SR/flags.sh"
# the runtime's sources by their names in gex, not this checkout's path
RTMAP="-ffile-prefix-map=$HERE=/gex/toolchain/gexcc"
clang $WASM_CFLAGS $RTMAP --sysroot="$PREFIX/sysroot" -c "$HERE/rt/main3.c" -o "$L/main3.o"
# -fgex-no-fork's object (rt/nofork.c): linked only into links that ask
clang $WASM_CFLAGS $RTMAP --sysroot="$PREFIX/sysroot" -c "$HERE/rt/nofork.c" -o "$L/nofork.o"
for f in "$HERE"/rt/poll.c "$HERE"/rt/sigsetjmp.c "$HERE"/rt/cxa_thread_atexit.c "$HERE"/rt/gshadow.c "$HERE"/rt/nlcompat.c "$HERE"/rt/fts.c "$HERE"/rt/gnu_basename.c "$HERE"/rt/shadow_r.c "$HERE"/rt/poll.s; do
  clang $WASM_CFLAGS $RTMAP --sysroot="$PREFIX/sysroot" -c "$f" -o "$PREFIX/build/rt/$(basename "$f").o"
done
rm -f "$PREFIX/sysroot/lib/libgexcc.a"
llvm-ar crs "$PREFIX/sysroot/lib/libgexcc.a" "$PREFIX"/build/rt/*.o
# libucontext (pkg-config libucontext): the <ucontext.h> functions, which
# fail (rt/ucontext.c)
clang $WASM_CFLAGS $RTMAP --sysroot="$PREFIX/sysroot" -c "$HERE/rt/ucontext.c" -o "$PREFIX/build/ucontext.o"
rm -f "$PREFIX/sysroot/lib/libucontext.a"
llvm-ar crs "$PREFIX/sysroot/lib/libucontext.a" "$PREFIX/build/ucontext.o"
printf 'libdir=/lib\nincludedir=/include\n\nName: libucontext\nDescription: ucontext functions (no stack switching on wasm32: they fail with ENOSYS)\nVersion: 1.3\nLibs: -L${libdir} -lucontext\nCflags:\n' \
  > "$PREFIX/sysroot/usr/lib/pkgconfig/libucontext.pc"
# the C++ exception tag: a static program's own; libc.so's for a shared
# program and its libraries, which import it (so not in libgexcc.a, which
# each module links)
mkdir -p "$PREFIX/build/rt-eh"
clang $WASM_CFLAGS $RTMAP -c "$HERE/rt/cpp_exception.s" -o "$PREFIX/build/rt-eh/cpp_exception.o"
rm -f "$PREFIX/sysroot/lib/libgexcc_eh.a"
llvm-ar crs "$PREFIX/sysroot/lib/libgexcc_eh.a" "$PREFIX/build/rt-eh/cpp_exception.o"
# an empty object: -shared's archives carry their deps in a copy of it
echo 'typedef int gexcc_empty;' | clang $WASM_CFLAGS -x c -c - -o "$L/empty.o"
GEXC=${GEXC:-}
[ -z "$GEXC" ] && [ -x "$ROOT/target/release/gexc" ] && GEXC=$ROOT/target/release/gexc
G=gexc
[ -n "$GEXC" ] && G=$(realpath "$GEXC")
# Containers run under gexcc-exec (below), which adds gexcc's defaults to gexc.
echo "$PREFIX/bin/gexcc-exec" > "$L/runner"
# -fgex-guest-i386's object (rt/i386.c), against the gex_abi.h of an x86
# gexc (GEX_X86: only it translates i386 code): GEXC_X86, else GEXC. Without
# one the flag is refused at link time.
rm -f "$L/i386.o"
GX=${GEXC_X86:-$GEXC}
if [ -n "$GX" ] && "$GX" cgen-c --header > "$PREFIX/build/gex_abi.h" 2>/dev/null &&
    grep -qx '#define GEX_X86 1' "$PREFIX/build/gex_abi.h"; then
  clang $WASM_CFLAGS $RTMAP --sysroot="$PREFIX/sysroot" -I"$HERE/include" -I"$PREFIX/build" \
    -I"$ROOT/crates/gex-cgen/providers" -c "$HERE/rt/i386.c" -o "$L/i386.o"
fi

# tool names autotools (--host=$TRIPLE), CMake and Meson look for
B=$PREFIX/bin
ln -sf gexcc "$B/gexc++"
for t in cc gcc clang; do ln -sf gexcc "$B/$TRIPLE-$t"; done
for t in c++ g++ clang++; do ln -sf gexcc "$B/$TRIPLE-$t"; done
# (the driver picks C++ by its name ending in ++)
for t in ar ranlib nm strings size readelf; do
  printf '#!/bin/sh\nexec llvm-%s "$@"\n' "$t" > "$B/$TRIPLE-$t"; chmod +x "$B/$TRIPLE-$t"
done
# objcopy: a gexcc program has no ELF interpreter, so its .interp, dumped
# (libcap's loader.txt: the loader a runnable library names), is empty
cat > "$B/$TRIPLE-objcopy" <<'OCEOF'
#!/bin/sh
case " $* " in
*" --dump-section .interp="*)
    out= in=
    for a; do
        case $a in
        .interp=*) out=${a#.interp=} ;;
        -*) ;;
        *) [ -z "$in" ] && [ -f "$a" ] && in=$a ;;
        esac
    done
    if [ -n "$out" ] && [ -n "$in" ] && ! llvm-readelf -S "$in" 2>/dev/null | grep -q ' \.interp '; then
        : > "$out"
        exit 0
    fi ;;
esac
exec llvm-objcopy "$@"
OCEOF
chmod +x "$B/$TRIPLE-objcopy"
# objdump: a side module described as GNU objdump describes an ELF shared
# object (Debian's dh_makeshlibs and dpkg-shlibdeps read SONAME and NEEDED)
cp "$HERE/gexcc-objdump" "$B/gexcc-objdump"
ln -sf gexcc-objdump "$B/$TRIPLE-objdump"
# a module's run path (dylink.0), rewritten at install as an ELF's is
cp "$HERE/gexcc-rpath" "$B/gexcc-rpath"
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
# pkg-config over the gexcc sysroot's libraries only (Debian's multiarch
# directory first). Static links name every library's own dependencies;
# with GEXCC_SHARED the libraries carry them.
export PKG_CONFIG_SYSROOT_DIR="$PREFIX/sysroot"
export PKG_CONFIG_LIBDIR="$PREFIX/sysroot/usr/lib/wasm32-linux-musl/pkgconfig:$PREFIX/sysroot/usr/lib/pkgconfig:$PREFIX/sysroot/usr/share/pkgconfig:$PREFIX/sysroot/usr/local/lib/pkgconfig"
unset PKG_CONFIG_PATH
st=; case \${GEXCC_SHARED:-0} in 0) st=--static ;; esac
# A variable naming a directory the sysroot does not have is where the
# package installs to (a udevdir), not a place to read from (a pkgdatadir
# of XML the build reads): the target's path, unprefixed. Which packages'
# variables are always target paths, though the sysroot has the directory
# (where others install to, or data a program finds at run time), only the
# builder knows: GEXCC_PKG_CONFIG_RUNTIME names them (space-separated).
case " \$* " in
*" --variable"*)
    v=\$(pkg-config \$st "\$@") || exit
    for p in \${GEXCC_PKG_CONFIG_RUNTIME:-}; do
        case " \$* " in *" \$p "*) v=\${v#"\$PKG_CONFIG_SYSROOT_DIR"} ;; esac
    done
    case \$v in
    "\$PKG_CONFIG_SYSROOT_DIR"/*) [ -e "\$v" ] || v=\${v#"\$PKG_CONFIG_SYSROOT_DIR"} ;;
    esac
    printf '%s\n' "\$v"
    exit 0 ;;
esac
exec pkg-config \$st "\$@"
EOF
chmod +x "$B/$TRIPLE-pkg-config"
ln -sf "$TRIPLE-pkg-config" "$B/gexcc-pkg-config"
# gexcc-exec: gexc for gexcc programs, with machine-wide slots (gexcc-exec.in)
sed -e "s|@GEXC@|$G|" -e "s|@LIBDIR@|$L|" "$HERE/gexcc-exec.in" > "$B/gexcc-exec"
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
# answers for configure's run-time checks in cross mode (CONFIG_SITE)
cp "$HERE/config.site" "$S/config.site"
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
# Debian's multiarch directory (usr/lib/wasm32-linux-musl), where its
# packages' libraries and CMake packages are
set(CMAKE_LIBRARY_ARCHITECTURE wasm32-linux-musl)
# Linked with their install run path from the start: CMake's install step
# rewrites an ELF's run path in place (file(RPATH_CHANGE)), which a wasm
# module is not (exiv2)
set(CMAKE_BUILD_WITH_INSTALL_RPATH ON CACHE BOOL "")
# A Debian package's build (dpkg-buildpackage exports DEB_HOST_ARCH) installs
# into its own DESTDIR; anything else installs into the sysroot.
if(NOT DEFINED ENV{DEB_HOST_ARCH})
  set(CMAKE_STAGING_PREFIX "$PREFIX/sysroot/usr" CACHE PATH "")
endif()
set(CMAKE_FIND_ROOT_PATH "$PREFIX/sysroot")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
# A Debian package's build: the build machine's tools' CMake packages too
# (extra-cmake-modules, /usr/share/ECM, is one, not the target's), after the
# sysroot's; the build machine's own libraries' are under its multiarch
# directory, which is not searched
if(DEFINED ENV{DEB_HOST_ARCH})
  set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE BOTH)
endif()
set(CMAKE_CROSSCOMPILING_EMULATOR "$B/gexcc-run")
set(PKG_CONFIG_EXECUTABLE "$B/$TRIPLE-pkg-config" CACHE FILEPATH "")
# Static libraries unless GEXCC_SHARED (wasm side modules)
if("\$ENV{GEXCC_SHARED}" STREQUAL "" OR "\$ENV{GEXCC_SHARED}" STREQUAL "0")
  set(BUILD_SHARED_LIBS OFF CACHE BOOL "")
endif()
# A file of cache settings that win over the command line's -D options
# (GEXCC_CMAKE_INIT; each set(... CACHE ... FORCE)): a packaging-level
# override of what a build script passes
if(DEFINED ENV{GEXCC_CMAKE_INIT})
  include("\$ENV{GEXCC_CMAKE_INIT}")
endif()
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
# (libc++ and busybox are built with the driver, its headers and runtime over
# that sysroot: their keys)
[ -z "$CK" ] || TK=$( { echo "$SK"; treesum "$HERE/gexcc" "$HERE/include" "$HERE/rt" "$HERE/config.site" "$SR/flags.sh"; } | hash16)
[ -z "$CK" ] || [ -z "${BUSYBOX_TAR:-}" ] || BK=$( { echo "$TK"; cat "$BUSYBOX_TAR"; } | hash16)
if [ -n "${BUSYBOX_TAR:-}" ] && [ -n "$CK" ] && [ -f "$CK/busybox-$BK/.done" ]; then
  cp "$CK/busybox-$BK/busybox" "$CK/busybox-$BK/busybox.applets" "$L/"
  echo "busybox: $L/busybox (cached: $CK/busybox-$BK)"
elif [ -n "${BUSYBOX_TAR:-}" ]; then
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
  if [ -n "$CK" ] && [ -s "$L/busybox.applets" ]; then
    rm -rf "$CK/busybox-$BK" && mkdir -p "$CK/busybox-$BK" &&
      cp "$L/busybox" "$L/busybox.applets" "$CK/busybox-$BK/" && touch "$CK/busybox-$BK/.done"
  fi
fi
# shared libraries (GEXCC_SHARED=1): gex-dylink (cargo
# build -p gex-dylink) and libc.so
DL=${GEX_DYLINK:-$ROOT/target/release/gex-dylink}
if [ -x "$DL" ]; then
  cp "$DL" "$L/gex-dylink"
  LK=
  [ -z "$CK" ] || LK=$( { echo "$TK v2"; treesum "$HERE/shared" "$DL" "$HERE/rt"; } | hash16)
  if [ -n "$LK" ] && [ -f "$CK/libc-so-$LK/.done" ]; then
    cp "$CK/libc-so-$LK/libc.so" "$CK/libc-so-$LK/libc_nonshared.a" "$PREFIX/sysroot/lib/" && cp "$CK/libc-so-$LK/dso.o" "$L/"
    echo "libc.so: $PREFIX/sysroot/lib/libc.so (cached: $CK/libc-so-$LK)"
  else
    "$HERE/shared/mklibc.sh" "$PREFIX"
    [ -z "$LK" ] || { rm -rf "$CK/libc-so-$LK" && mkdir -p "$CK/libc-so-$LK" &&
      cp "$PREFIX/sysroot/lib/libc.so" "$PREFIX/sysroot/lib/libc_nonshared.a" "$L/dso.o" "$CK/libc-so-$LK/" &&
      touch "$CK/libc-so-$LK/.done"; }
  fi
else
  echo "gexcc: no $DL: GEXCC_SHARED unavailable (cargo build --release -p gex-dylink)"
fi
# What the toolchain was installed from, so a package built with it can be
# told apart from one built before a fix: the gex commit (with "+" when the
# tree had changes) and the musl one.
{
  echo "gex $(git -C "$ROOT" rev-parse HEAD 2>/dev/null || echo unknown)$(git -C "$ROOT" diff --quiet HEAD -- toolchain/gexcc 2>/dev/null || echo +)"
  echo "gex-repo $ROOT"
  echo "musl $(git -C "$MUSL" rev-parse HEAD 2>/dev/null || echo unknown)"
} > "$PREFIX/lib/gexcc/installed-from"
echo "gexcc: $B/gexcc (sysroot $PREFIX/sysroot, runner $R)"
# C++ (LLVM_SRC: a checkout of toolchain/pins' LLVM_COMMIT)
if [ -n "${LLVM_SRC:-}" ] && [ -n "$CK" ]; then
  XK=$( { echo "$TK $(git -C "$LLVM_SRC" rev-parse HEAD)"; git -C "$LLVM_SRC" status --porcelain --untracked-files=no
    treesum "$HERE/build-cxx.sh" "$HERE/llvm-pin.sh"; } | hash16)
  if [ -f "$CK/cxx-$XK.tar" ]; then
    tar -xf "$CK/cxx-$XK.tar" -C "$PREFIX/sysroot/"
    echo "libc++: $PREFIX/sysroot/lib/libc++.a (cached: $CK/cxx-$XK.tar)"
  else
    "$HERE/build-cxx.sh" "$PREFIX" "$LLVM_SRC"
    # what its install wrote (CMake's manifest), by path in the sysroot
    SR=$(realpath "$PREFIX")/sysroot
    sed "s|^$SR/||" "$PREFIX/build/cxx/install_manifest.txt" > "$PREFIX/build/cxx.files"
    tar -cf "$CK/cxx-$XK.tar.tmp" -C "$SR/" -T "$PREFIX/build/cxx.files" && mv "$CK/cxx-$XK.tar.tmp" "$CK/cxx-$XK.tar"
  fi
elif [ -n "${LLVM_SRC:-}" ]; then
  "$HERE/build-cxx.sh" "$PREFIX" "$LLVM_SRC"
fi
