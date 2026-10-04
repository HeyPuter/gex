# Shared by the wine-wasm scripts: clang's flags for wasm32 code that runs as
# a gex native module (position-independent, shared memory, one instance per
# thread). Kept to the features V8 and wasmtime 27 both have.
WASM_TARGET=wasm32-unknown-linux-musl
WASM_FEATURES="-matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint -mtail-call"
WASM_CFLAGS="--target=$WASM_TARGET -O2 -fPIC $WASM_FEATURES -fno-exceptions"
