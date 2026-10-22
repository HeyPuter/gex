# The wasm exception tag C++ throws and catches with (LLVM's C++ EH lowering
# and libunwind's Unwind-wasm.c refer to it; the runtime defines it, as
# emscripten's libcxxabi/src/__cpp_exception.S does). wasm32: the payload is
# a pointer, an i32. In libgexcc_eh.a for a static program, and in libc.so
# for a shared one and its libraries, which import it: an exception thrown
# in one module is caught in another (with a tag of each module's own it
# would be a different exception there).
	.globl	__cpp_exception
	.tagtype	__cpp_exception i32
__cpp_exception:
