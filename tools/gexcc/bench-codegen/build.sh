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
LDC="--import-memory --shared-memory --max-memory=4294967296 --no-entry"
objs wasm32 -fno-PIC $O/o32s
objs wasm32 "-fPIC -fvisibility=default" $O/o32p
objs wasm32 -fPIC $O/o32h
objs wasm64 -fno-PIC $O/o64s
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
# asyncify-ind: every function that can reach an indirect call instrumented
# (a shared library built without knowing its callers' callbacks);
# asyncify-all: every function
$OPT $O/pie.wasm --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext --enable-nontrapping-float-to-int --asyncify --pass-arg=asyncify-ignore-imports -O2 -o $O/asyncify-ind.wasm
$OPT $O/pie.wasm --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext --enable-nontrapping-float-to-int --asyncify '--pass-arg=asyncify-addlist@*' -O2 -o $O/asyncify-all.wasm
# and the PIE through the same wasm-opt -O2 (fair against asyncify-all)
$OPT $O/pie.wasm --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext --enable-nontrapping-float-to-int -O2 -o $O/pie-opt.wasm
ls -la $O/*.wasm
