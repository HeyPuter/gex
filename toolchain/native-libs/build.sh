#!/bin/bash
# A native library: Debian's zlib as an LP64 wasm library that
# replaces libz.so.1 under an aarch64 program. Compiled for wasm64 (LP64:
# long, pointers and size_t 64-bit, as aarch64's), linked position-
# independent with adapters for each export (gen.py) and a runtime (lib.sh:
# the LP64 libc with A64_SYSROOT, else rt.c), then lowered to the guest's
# 32-bit memory (binaryen's --memory64-lowering: every address wrapped to
# i32 at its access). With the libc, gz* (file I/O) is in too.
#
#   build.sh OUT   (ZLIB_SRC: Debian's zlib source, unpacked;
#                   GEXCC_PREFIX: the gexcc toolchain, for wasm-opt;
#                   A64_SYSROOT; DEBIAN_ROOT for the test program)
set -eu
H=$(cd "$(dirname "$0")" && pwd); O=$(realpath -m "$1"); mkdir -p "$O"
Z=${ZLIB_SRC:?the Debian zlib source (1.3.1), unpacked}
OPT=${GEXCC_PREFIX:?}/sysroot/binaryen/bin/wasm-opt
. "$H/lib.sh"
lib_setup "$H" "$O"
ZF="-D_LARGEFILE64_SOURCE=1 -D_LFS64_LARGEFILE=1 -I$Z"
srcs="adler32 crc32 deflate inflate inftrees inffast trees zutil compress uncompr infback"
if [ -n "${A64_SYSROOT:-}" ]; then
  srcs="$srcs gzlib gzread gzwrite gzclose"; GZ=--gz; ZF="$ZF -DHAVE_UNISTD_H -DHAVE_STDARG_H"
else
  GZ=; ZF="$ZF -Doff64_t=long"
fi
python3 "$H/gen.py" zlib $GZ - "$Z/zlib.h" > "$O/adapters.c"
objs=()
for f in $srcs; do
  clang $CF $ZF -include "$H/zalloc.h" -c "$Z/$f.c" -o "$O/z_$f.o" &
  objs+=("$O/z_$f.o")
done
clang $CF $ZF -I"$H" -c "$O/adapters.c" -o "$O/adapters.o" &
clang $CF $ZF -I"$H" -c "$H/zlib-alloc.c" -o "$O/zlib-alloc.o" &
wait
lib_link "$O" libz.so.1 "${objs[@]}" "$O/adapters.o" "$O/zlib-alloc.o"
# the test program, an arm64 binary against Debian's libz.so.1 (DEBIAN_ROOT:
# an arm64 root with usr/lib/aarch64-linux-gnu/libz.so.1)
if [ -n "${DEBIAN_ROOT:-}" ]; then
  clang --target=aarch64-linux-gnu -O2 -ffreestanding -fno-builtin -fPIE -c "$H/zt.c" -o "$O/zt.o"
  ld.lld -pie -dynamic-linker /lib/ld-linux-aarch64.so.1 -e _start --allow-shlib-undefined -o "$O/zt" "$O/zt.o" \
    "$DEBIAN_ROOT/usr/lib/aarch64-linux-gnu/libz.so.1"
  ls -l "$O/zt"
fi
