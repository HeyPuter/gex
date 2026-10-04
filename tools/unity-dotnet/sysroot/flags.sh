# clang's flags for wasm64 code that runs as a gex stateful provider inside
# an x86-64 guest process (position-independent, shared memory64, one
# instance per thread). clang has no wasm64 Linux triple, so the target is
# wasm64-unknown-unknown with Linux's predefined macros added.
WASM64_TARGET="--target=wasm64-unknown-unknown -D__linux__=1 -D__linux=1 -D__unix__=1 -D__unix=1"
WASM64_FEATURES="-matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint -mtail-call"
WASM64_CFLAGS="$WASM64_TARGET -O2 -fPIC $WASM64_FEATURES -fno-exceptions"
