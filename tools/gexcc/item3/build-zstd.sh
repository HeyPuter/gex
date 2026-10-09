#!/bin/bash
# item 3 for zstd: Debian's libzstd as an LP64 wasm library that replaces
# libzstd.so.1 under an aarch64 program, as build.sh does zlib (gen.py's
# adapters, rt.c, the lowering), and its test program zstt.
#
# Single-threaded: built without ZSTD_MULTITHREAD (Debian's has it), so
# ZSTD_c_nbWorkers > 0 is refused as an unsupported parameter. No
# dictionary builder (ZDICT_*: qsort_r, clock and stdio; they are bound to
# gexp__missing) and no legacy formats.
#
#   build-zstd.sh OUT   (GEXCC_PREFIX; DEBIAN_ROOT for zstt)
set -eu
H=$(cd "$(dirname "$0")" && pwd); O=$(realpath -m "$1"); mkdir -p "$O/zstd"
S=${ZSTD_SRC:-$HOME/gex-sandbox/scratch/CC/debian/src/libzstd-1.5.7+dfsg-1}/lib
OPT=${GEXCC_PREFIX:?}/sysroot/binaryen/bin/wasm-opt
INC=$H/../bench-codegen/src/inc
CF="--target=wasm64-unknown-unknown -D__linux__=1 -DZSTD_DISABLE_ASM -DZSTD_LEGACY_SUPPORT=0 -DZSTD_STATIC_LINKING_ONLY -DZDICT_STATIC_LINKING_ONLY -DXXH_NAMESPACE=ZSTD_ -O2 -fPIC -matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint -ffreestanding -nostdinc -isystem $(clang -print-resource-dir)/include -isystem $INC -w"
# A copy of the sources with the custom allocator's calls going through
# zstd-alloc.c: a caller's customAlloc/customFree is guest code.
rm -rf "$O/zstd/src"; cp -r "$S" "$O/zstd/src"; L=$O/zstd/src
python3 - "$L/common/allocations.h" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
for a, b in [("customMem.customAlloc(customMem.opaque, size)", "gexp_zstd_alloc(customMem, size)"),
             ("customMem.customFree(customMem.opaque, ptr)", "gexp_zstd_free(customMem, ptr)")]:
    assert a in s, a
    s = s.replace(a, b)
s = s.replace("MEM_STATIC void* ZSTD_customMalloc", "void *gexp_zstd_alloc(ZSTD_customMem m, size_t size);\nvoid gexp_zstd_free(ZSTD_customMem m, void *p);\nMEM_STATIC void* ZSTD_customMalloc", 1)
open(p, "w").write(s)
PY
python3 "$H/gen.py" zstd - "$L/zstd.h" "$L/zstd_errors.h" > "$O/zstd/adapters.c"
objs=()
for f in common/debug common/pool common/entropy_common common/error_private common/fse_decompress common/xxhash common/zstd_common \
  compress/fse_compress compress/hist compress/huf_compress compress/zstd_compress compress/zstd_compress_literals \
  compress/zstd_compress_sequences compress/zstd_compress_superblock compress/zstd_double_fast compress/zstd_fast \
  compress/zstd_lazy compress/zstd_ldm compress/zstd_opt compress/zstd_preSplit \
  decompress/huf_decompress decompress/zstd_ddict decompress/zstd_decompress decompress/zstd_decompress_block; do
  o=$O/zstd/$(echo $f | tr / _).o; objs+=("$o")
  clang $CF -I"$L" -I"$L/common" -c "$L/$f.c" -o "$o" &
done
clang $CF -I"$H" -I"$L" -I"$L/common" -c "$O/zstd/adapters.c" -o "$O/zstd/adapters.o" &
clang $CF -I"$H" -c "$H/rt.c" -o "$O/zstd/rt.o" &
clang $CF -I"$H" -I"$L" -I"$L/common" -c "$H/zstd-alloc.c" -o "$O/zstd/zstd-alloc.o" &
clang --target=wasm64-unknown-unknown -c "$H/tls.s" -o "$O/zstd/tls.o" &
wait
wasm-ld -mwasm64 --experimental-pic -pie --no-entry --shared-memory --import-memory --max-memory=4294967296 \
  --export=gexp_table -o "$O/libzstd-64.wasm" "${objs[@]}" "$O"/zstd/{adapters,rt,zstd-alloc,tls}.o
$OPT "$O/libzstd-64.wasm" --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext \
  --enable-nontrapping-float-to-int --enable-memory64 --memory64-lowering -O2 -g -o "$O/libzstd.so.1.wasm"
ls -l "$O/libzstd.so.1.wasm"
if [ -n "${DEBIAN_ROOT:-}" ]; then
  clang --target=aarch64-linux-gnu -O2 -ffreestanding -fno-builtin -fPIE -I"$S" -c "$H/zstt.c" -o "$O/zstt.o"
  ld.lld -pie -dynamic-linker /lib/ld-linux-aarch64.so.1 -e _start --allow-shlib-undefined -o "$O/zstt" "$O/zstt.o" \
    "$DEBIAN_ROOT/usr/lib/aarch64-linux-gnu/libzstd.so.1"
  ls -l "$O/zstt"
fi
