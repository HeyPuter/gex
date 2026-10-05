# Shared by the wine-wasm scripts: clang's flags for wasm32 code that runs as
# a gex native module (position-independent, shared memory, one instance per
# thread). Kept to the features V8 and wasmtime 27 both have. No tail calls:
# binaryen's asyncify (fork, wcc) cannot instrument them, and the libc is
# shared by every program.
WASM_TARGET=wasm32-unknown-linux-musl
WASM_FEATURES="-matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint"
# setjmp/longjmp lower onto wasm exception handling (V8 has it; wasmtime 27,
# gex's native engine, does not: a module that uses setjmp needs the
# wasmtime upgrade to run natively). clang gives a wasm32-*-linux target no
# wasm exception model by default, so it is asked for (-exception-model=wasm),
# or the lowering emits no catch.
WASM_CFLAGS="--target=$WASM_TARGET -O2 -fPIC $WASM_FEATURES -fno-exceptions -mexception-handling -mllvm -wasm-enable-sjlj -mllvm -wasm-use-legacy-eh=false -Xclang -exception-model=wasm"
