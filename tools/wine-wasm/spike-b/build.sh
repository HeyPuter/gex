#!/bin/sh
# W0(b): demo.dll as a wasm module plus a PE skeleton, and an i386 test
# program. OUT gets demo.wasm, demo.dll, test.exe.
#   tools/wine-wasm/spike-b/build.sh OUT GEX_ABI_H_DIR
# (GEX_ABI_H_DIR holds gex_abi.h: `gexc cgen-c --header > DIR/gex_abi.h`)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
OUT=$1 ABI=$2
mkdir -p "$OUT"
F="--target=wasm32 -O2 -fPIC -matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint -mtail-call
   -ffreestanding -nostdlib -fshort-wchar -D_WIN32 -Wno-ignored-attributes"
LD="wasm-ld --experimental-pic -shared --shared-memory --import-memory --max-memory=4294967296 --no-entry"
# 1. the probe: the DLL's own code, for its exports' wasm signatures
clang $F -I"$here" -c "$here/demo.c" -o "$OUT/demo.o"
$LD --export-all --unresolved-symbols=import-dynamic "$OUT/demo.o" -o "$OUT/probe.wasm"
# 2. adapters, exit stubs, skeleton source
python3 "$root/tools/wine-wasm/specgen.py" "$here/demo.spec" "$OUT/probe.wasm" "$OUT/demo" \
  --source "$here/demo.c" --exit i:pp --exit i:i
# 3. the module: DLL code and adapters in one unit (the per-thread globals
#    of icall.h are static to it), and the per-thread globals
clang $F -I"$here" -I"$ABI" -I"$root/crates/gex-cgen/providers" -c "$OUT/demo.adapters.c" -o "$OUT/demo.adapters.o"
clang --target=wasm32 -c "$root/tools/wine-wasm/gexw_globals.s" -o "$OUT/gexw_globals.o"
$LD "$OUT/demo.adapters.o" "$OUT/gexw_globals.o" -o "$OUT/demo.wasm"
# 4. the PE skeleton and the test program
i686-w64-mingw32-gcc -O2 -s -shared -nostdlib -Wl,--no-insert-timestamp -Wl,-e,_skel_entry \
  -o "$OUT/demo.dll" "$OUT/demo.skel.c" "$OUT/demo.def"
i686-w64-mingw32-gcc -O2 -s -Wl,--no-insert-timestamp -o "$OUT/test.exe" "$here/test.c"
rm -f "$OUT/demo.cwasm"  # libprov caches the compiled module beside it
