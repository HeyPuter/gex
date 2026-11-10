# Shared by build.sh and build-zstd.sh: how an item-3 library compiles and
# links. With A64_SYSROOT (sysroot.sh's OUT/sysroot) it builds against the
# LP64 libc (musl's wasm64a64 arch: malloc, stdio, the rest of libc, and
# syscalls on the calling guest thread); without, against rt.c's few
# freestanding functions.
#   lib_setup H O    sets CF (clang flags), RT_OBJS (the runtime's objects,
#                    compiled into O), LIBS (link inputs after the objects)
lib_setup() {
  local h=$1 o=$2 res; res=$(clang -print-resource-dir)/include
  . "$h/flags.sh"
  if [ -n "${A64_SYSROOT:-}" ]; then
    CF="$A64W_CFLAGS -DA64_MUSL -nostdinc -isystem $A64_SYSROOT/include -isystem $res -w"
    RT_SRCS="a64call.c rtmusl.c"
    LIBS="$A64_SYSROOT/lib/libc.a $A64_SYSROOT/lib/libclang_rt.builtins-wasm64.a"
  else
    CF="$A64W_CFLAGS -ffreestanding -nostdinc -isystem $res -isystem $h/inc -w"
    RT_SRCS="a64call.c rt.c"
    LIBS=
  fi
  # no path of the build machine's in the library (an assert's __FILE__):
  # the output directory and each library's source tree by names of their own
  CF="$CF -ffile-prefix-map=$(realpath -m "$o")=."
  local s
  for s in "${ZLIB_SRC:-}" "${ZSTD_SRC:-}" "${SQLITE_SRC:-}"; do
    [ -n "$s" ] && CF="$CF -ffile-prefix-map=$(realpath -m "$s")=src"
  done
  RT_OBJS=
  for f in $RT_SRCS; do
    clang $CF -I"$h" -c "$h/$f" -o "$o/rt_${f%.c}.o" &
    RT_OBJS="$RT_OBJS $o/rt_${f%.c}.o"
  done
  clang --target=wasm64-unknown-unknown -c "$h/tls.s" -o "$o/rt_tls.o" &
  RT_OBJS="$RT_OBJS $o/rt_tls.o"
  wait
}
# lib_link O NAME OBJS...: link, then lower to the guest's 32-bit memory.
lib_link() {
  local o=$1 name=$2; shift 2
  wasm-ld -mwasm64 --experimental-pic -pie --no-entry --shared-memory --import-memory --max-memory=4294967296 \
    --export=gexp_table --export-if-defined=__gexa_entries -o "$o/$name-64.wasm" "$@" $RT_OBJS $LIBS
  $OPT "$o/$name-64.wasm" --enable-threads --enable-bulk-memory --enable-mutable-globals --enable-sign-ext \
    --enable-nontrapping-float-to-int --enable-memory64 --memory64-lowering -O2 -g -o "$o/$name.wasm"
  ls -l "$o/$name.wasm"
}
