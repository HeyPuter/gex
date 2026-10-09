# clang's flags for item 3's LP64 wasm code (an aarch64 guest's library):
# wasm64 with Linux's predefined macros (clang has no wasm64 Linux triple),
# aarch64's unsigned char, position-independent, shared memory.
A64W_TARGET="--target=wasm64-unknown-unknown -D__linux__=1 -D__linux=1 -D__unix__=1 -D__unix=1 -funsigned-char"
A64W_FEATURES="-matomics -mbulk-memory -mmutable-globals -msign-ext -mnontrapping-fptoint"
A64W_CFLAGS="$A64W_TARGET -O2 -fPIC $A64W_FEATURES -fno-exceptions"
