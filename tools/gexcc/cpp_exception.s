# The wasm exception tag C++ throws and catches with (LLVM's C++ EH lowering
# and libunwind's Unwind-wasm.c refer to it; the runtime defines it, as
# emscripten's libcxxabi/src/__cpp_exception.S does). wasm32: the payload is
# a pointer, an i32.
	.globl	__cpp_exception
	.tagtype	__cpp_exception i32
__cpp_exception:
