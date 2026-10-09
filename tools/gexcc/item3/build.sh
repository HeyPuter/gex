#!/bin/bash
# item 3 (docs/gexcc.md §7.8): Debian's zlib as an LP64 wasm library that
# replaces libz.so.1 under an aarch64 program. Compiled for wasm64 (LP64:
# long, pointers and size_t 64-bit, as aarch64's), linked position-
# independent with adapters for each export (gen.py) and a small runtime
# (rt.c), then lowered to the guest's 32-bit memory (binaryen's
# --memory64-lowering: every address wrapped to i32 at its access).
#
#   build.sh OUT   (GEXCC_PREFIX: the gexcc toolchain, for wasm-opt)
set -eu
H=$(cd "$(dirname "$0")" && pwd); O=$(realpath -m "$1"); mkdir -p "$O"
Z=${ZLIB_SRC:-$HOME/gex-sandbox/scratch/CC/debian/src/zlib-1.3.dfsg+really1.3.1-1}
OPT=${GEXCC_PREFIX:?}/sysroot/binaryen/bin/wasm-opt
INC=$H/../bench-codegen/src/inc
CF="--target=wasm64-unknown-unknown -D__linux__=1 -D_LARGEFILE64_SOURCE=1 -D_LFS64_LARGEFILE=1 -Doff64_t=long -O2 -fPIC -matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint -ffreestanding -nostdinc -isystem $(clang -print-resource-dir)/include -isystem $INC -I$Z -w"
# an adapter for every function zlib.h declares (but gz*, which need stdio)
python3 "$H/gen.py" zlib - "$Z/zlib.h" > "$O/adapters.c"
for f in adler32 crc32 deflate inflate inftrees inffast trees zutil compress uncompr infback; do
  clang $CF -include "$H/zalloc.h" -c "$Z/$f.c" -o "$O/z_$f.o" &
done
clang $CF -I"$H" -c "$O/adapters.c" -o "$O/adapters.o" &
clang $CF -I"$H" -c "$H/rt.c" -o "$O/rt.o" &
clang $CF -I"$H" -c "$H/zlib-alloc.c" -o "$O/zlib-alloc.o" &
clang --target=wasm64-unknown-unknown -c "$H/tls.s" -o "$O/tls.o" &
wait
wasm-ld -mwasm64 --experimental-pic -pie --no-entry --shared-memory --import-memory --max-memory=4294967296 \
  --export=gexp_table -o "$O/libz-64.wasm" "$O"/z_*.o "$O"/adapters.o "$O"/rt.o "$O"/zlib-alloc.o "$O"/tls.o
$OPT "$O/libz-64.wasm" --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext \
  --enable-nontrapping-float-to-int --enable-memory64 --memory64-lowering -O2 -g -o "$O/libz.so.1.wasm"
ls -l "$O/libz.so.1.wasm"
# the test program, an arm64 binary against Debian's libz.so.1 (DEBIAN_ROOT:
# an arm64 root with usr/lib/aarch64-linux-gnu/libz.so.1)
if [ -n "${DEBIAN_ROOT:-}" ]; then
  clang --target=aarch64-linux-gnu -O2 -ffreestanding -fno-builtin -fPIE -c "$H/zt.c" -o "$O/zt.o"
  ld.lld -pie -dynamic-linker /lib/ld-linux-aarch64.so.1 -e _start --allow-shlib-undefined -o "$O/zt" "$O/zt.o" \
    "$DEBIAN_ROOT/usr/lib/aarch64-linux-gnu/libz.so.1"
  ls -l "$O/zt"
fi
