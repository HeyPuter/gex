# 64-bit Wine: clang's flags for LP64 code that runs as
# a gex native program after lowering to wasm32 (wcc64): wasm64 with Linux's
# predefined macros (clang has no wasm64 Linux triple), x86-64's signed char
# and layouts, position-independent, shared memory; setjmp lowered onto wasm
# exception handling as on wasm32 (flags.sh). No tail calls (as wasm32).
WASM64_TARGET="--target=wasm64-unknown-unknown -D__linux__=1 -D__linux=1 -Dlinux=1 -D__unix__=1 -D__unix=1 -Dunix=1 -D__gnu_linux__=1 -D__ELF__=1"
WASM64_FEATURES="-matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint"
WASM64_CFLAGS="$WASM64_TARGET -O2 -fPIC $WASM64_FEATURES -fno-exceptions -mexception-handling -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false -Xclang -exception-model=wasm"
