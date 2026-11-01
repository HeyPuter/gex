# The wasm exception tag C++ throws and catches with (a program's own: the
# Wine core has no C++). wasm64: the payload is a pointer, an i64.
	.globl	__cpp_exception
	.tagtype	__cpp_exception i64
__cpp_exception:
