#!/bin/sh
# libc.so for gexcc's shared programs: musl, libgex and
# libgexcc as one wasm side module, linked by gexcc itself (GEXCC_SHARED) so
# it gets the same finishing as every library (self-binding, asyncify for
# fork, its gex.forkreach summary). Also libc_nonshared.a (what stays in the
# program) and dso.o (a module's __tls_base setter).
#
#   toolchain/gexcc/shared/mklibc.sh PREFIX
set -eu
PREFIX=$1
HERE=$(dirname "$(realpath "$0")")
L=$PREFIX/sysroot/lib
W=$PREFIX/build/libc-so
rm -rf "$W"; mkdir -p "$W/lc" "$W/gx" "$W/ns"
. "$HERE/../../sysroot/flags.sh"
clang $WASM_CFLAGS -c "$HERE/dso.s" -o "$PREFIX/lib/gexcc/dso.o"
# musl's archive repeats member names: extracted under unique ones
python3 "$HERE/arx.py" "$L/libc.a" "$W/lc"
(cd "$W/gx" && llvm-ar x "$L/libgex.a" && llvm-ar x "$L/libgexcc.a" && llvm-ar x "$L/libgexcc_eh.a")
# the loop safepoint's countdown is every module's own (gexcc links
# libgexcc.a's into each)
rm -f "$W"/gx/poll.*
# libc objects that define what libgex or libgexcc define go
llvm-nm -j --defined-only --extern-only "$W"/gx/*.o 2>/dev/null | grep -v ':$' | sort -u > "$W/gexsyms"
for o in "$W"/lc/*; do
  if llvm-nm -j --defined-only --extern-only "$o" 2>/dev/null | grep -qxFf "$W/gexsyms"; then rm "$o"; fi
done
# main(void)'s shim calls the program's main: it stays in the program
mv "$W"/lc/*-__main_argc_argv.lo "$W/ns/"
rm -f "$L/libc_nonshared.a"
llvm-ar crs "$L/libc_nonshared.a" "$W"/ns/*
# musl's asynchronous-cancel labels: wasm has no pc inside a system call
printf '__attribute__((visibility("hidden"))) const char __cp_begin[1], __cp_end[1], __cp_cancel[1];\n' > "$W/cp.c"
clang $WASM_CFLAGS -c "$W/cp.c" -o "$W/gx/cp.o"
# libc calls the program's entry (crt1's __gex_start_c), bound at load
printf '__gex_start_c\n' > "$W/undef.txt"
GEXCC_SHARED=1 GEXCC_POLL=0 "$PREFIX/bin/gexcc" -shared -nostdlib -o "$L/libc.so" \
  -Wl,-soname,libc.so -Wl,--export=__gex_fork_run -Wl,--allow-undefined-file="$W/undef.txt" \
  "$W"/lc/* "$W"/gx/*.o "$L/libclang_rt.builtins-wasm32.a"
# the builtins libc's own code takes (128-bit integers, long double) are
# every library's: one built without -fvisibility=default hides them, and a
# library that imports them from libc fails to load
for s in __multi3 __addtf3 __trunctfdf2; do
  llvm-nm --extern-only --defined-only "$L/libc.so" | grep -q " $s\$" ||
    { echo "mklibc.sh: libc.so does not export $s (compiler-rt's builtins built hidden: $L/libclang_rt.builtins-wasm32.a)" >&2; exit 1; }
done
echo "libc.so: $(wc -c < "$L/libc.so") bytes"
