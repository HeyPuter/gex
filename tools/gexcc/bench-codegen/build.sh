#!/bin/bash
# Codegen variants for docs/gexcc.md §7.3: the same kernels and runtime as
# static, PIE (gexcc today), split (libc a side module), lib (kernels in a
# side module, as a shared library), lp64 (wasm64 lowered to wasm32), and
# asyncify-all (every function instrumented, as a conservatively built
# shared library would be).
set -eu
D=$(cd "$(dirname "$0")" && pwd); S=$D/src; O=$D/out; mkdir -p $O
Z=${ZLIB_SRC:?zlib 1.3.1 source (configured)}
ZS=${ZSTD_SRC:?zstd 1.5.7 source}/lib
OPT=${GEXCC_PREFIX:?}/sysroot/binaryen/bin/wasm-opt
FE="-matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint"
CF="-O2 -ffreestanding -nostdinc -isystem $(clang -print-resource-dir)/include -isystem $S/inc -fno-builtin-malloc -DZSTD_DISABLE_ASM -DZSTD_NO_INTRINSICS -DXXH_NAMESPACE=ZSTD_ -DDEBUGLEVEL=0 -I$Z -I$ZS -I$ZS/common -w"
ZF="adler32 crc32 deflate inflate inftrees inffast trees zutil compress uncompr"
ZSF=$(cd $ZS && ls common/*.c compress/*.c decompress/*.c)
objs() { # TARGET PICFLAG OUTDIR
  local t=$1 pic=$2 od=$3; mkdir -p $od
  for f in $ZF; do clang --target=$t $FE $CF $pic -c $Z/$f.c -o $od/z_$f.o & done; wait
  for f in $ZSF; do clang --target=$t $FE $CF $pic -c $ZS/$f -o $od/zs_$(echo $f | tr / _).o & done; wait
  clang --target=$t $FE $CF $pic -c $S/kernels.c -o $od/kernels.o
  clang --target=$t $FE $CF $pic -c $S/minilibc.c -o $od/minilibc.o
}
WF="--enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext --enable-nontrapping-float-to-int"
LDC="--import-memory --shared-memory --max-memory=4294967296 --no-entry"
objs wasm32 -fno-PIC $O/o32s
objs wasm32 "-fPIC -fvisibility=default" $O/o32p
objs wasm32 -fPIC $O/o32h
objs wasm64 -fno-PIC $O/o64s
# wasm32 with LP64's 64-bit size_t and ptrdiff_t (pointers stay 32-bit): the
# i64 arithmetic of the lp64 build without its wider pointers and addresses
objs wasm32 "-fno-PIC -include $S/size64.h" $O/o32z
K="$O/o32s/z_*.o $O/o32s/zs_*.o $O/o32s/kernels.o"
wasm-ld $LDC --global-base=1048576 --export=bench_run -o $O/static.wasm $O/o32s/*.o
# pie: gexcc today (hidden visibility); pie-dv: the same objects a shared library build makes
wasm-ld $LDC --experimental-pic -pie --export=bench_run -o $O/pie.wasm $O/o32h/*.o
wasm-ld $LDC --experimental-pic -pie --export=bench_run -o $O/pie-dv.wasm $O/o32p/*.o
# split: libc in a side module, the rest a PIE main importing it
wasm-ld $LDC --experimental-pic -shared -o $O/libc.so.wasm $O/o32p/minilibc.o
wasm-ld $LDC --experimental-pic -pie --export=bench_run --unresolved-symbols=import-dynamic -o $O/split.wasm $(ls $O/o32p/*.o | grep -v minilibc)
# lib: kernels and libc in one side module; main is a stub calling it
printf 'unsigned bench_run(int,int);\nunsigned run(int k,int n){return bench_run(k,n);}\n' > $O/stub.c
clang --target=wasm32 $FE -O2 -fPIC -c $O/stub.c -o $O/stub.o
wasm-ld $LDC --experimental-pic -shared -o $O/libk.so.wasm $(ls $O/o32p/*.o)
wasm-ld $LDC --experimental-pic -pie --export=run --unresolved-symbols=import-dynamic -o $O/lib.wasm $O/stub.o
# lp64: wasm64 static, lowered to a wasm32 memory
wasm-ld -mwasm64 $LDC --global-base=1048576 --export=bench_run -o $O/lp64-64.wasm $O/o64s/*.o
$OPT $O/lp64-64.wasm --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext --enable-nontrapping-float-to-int --enable-memory64 --memory64-lowering -O2 -o $O/lp64.wasm
# the same lowering without binaryen's -O2 (static has none), the static
# build through -O2 (lp64's fair baseline), and wasm64 left unlowered
# (V8's memory64)
$OPT $O/lp64-64.wasm $WF --enable-memory64 --memory64-lowering -o $O/lp64-noopt.wasm
$OPT $O/static.wasm $WF -O2 -o $O/static-opt.wasm
# lp64-pre: -O2 on the wasm64 module first (while addresses are still
# addresses), then lowered, then -O2 again
$OPT $O/lp64-64.wasm $WF --enable-memory64 -O2 --memory64-lowering -O2 -o $O/lp64-pre.wasm
cp $O/lp64-64.wasm $O/w64.wasm
wasm-ld $LDC --global-base=1048576 --export=bench_run -o $O/size64.wasm $O/o32z/*.o
# the kernels as a native aarch64 Linux program, run translated by gex
# (run.sh: tier 1 and the function JIT, in the browser shim under node)
A64="--target=aarch64-linux-gnu -O2 -ffreestanding -nostdinc -isystem $(clang -print-resource-dir)/include -isystem $S/inc -fno-builtin-malloc -DZSTD_DISABLE_ASM -DZSTD_NO_INTRINSICS -DXXH_NAMESPACE=ZSTD_ -DDEBUGLEVEL=0 -I$Z -I$ZS -I$ZS/common -w"
mkdir -p $O/a64
for f in $ZF; do clang $A64 -c $Z/$f.c -o $O/a64/z_$f.o & done; wait
for f in $ZSF; do clang $A64 -c $ZS/$f -o $O/a64/zs_$(echo $f | tr / _).o & done; wait
for f in kernels a64main; do clang $A64 -c $S/$f.c -o $O/a64/$f.o; done
clang $A64 -fno-builtin -c $S/minilibc.c -o $O/a64/minilibc.o
for f in memcpy memset; do clang --target=aarch64-linux-gnu -c $S/a64/$f.S -o $O/a64/$f.o; done
# separate-code: gex wants the text segment on pages of its own (lld packs
# it after rodata on a shared page by default, which gex then misses)
ld.lld -static -z separate-code -e _start --image-base=0x400000 -o $O/kernels.a64 $O/a64/*.o
# asyncify-ind: every function that can reach an indirect call instrumented
# (a shared library built without knowing its callers' callbacks);
# asyncify-all: every function
$OPT $O/pie.wasm --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext --enable-nontrapping-float-to-int --asyncify --pass-arg=asyncify-ignore-imports -O2 -o $O/asyncify-ind.wasm
$OPT $O/pie.wasm --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext --enable-nontrapping-float-to-int --asyncify '--pass-arg=asyncify-addlist@*' -O2 -o $O/asyncify-all.wasm
# and the PIE through the same wasm-opt -O2 (fair against asyncify-all)
$OPT $O/pie.wasm --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext --enable-nontrapping-float-to-int -O2 -o $O/pie-opt.wasm
ls -la $O/*.wasm
