# stackRestore for the runtime wasm JIT (docs/unity-dotnet.md P5): emscripten's
# compiler-rt helper that sets __stack_pointer, which interp.c calls to drop
# the C-stack frames of JIT code an exception abandoned. wasm64.
	.globaltype	__stack_pointer, i64
	.globl	stackRestore
	.type	stackRestore,@function
stackRestore:
	.functype	stackRestore (i64) -> ()
	local.get	0
	global.set	__stack_pointer
	end_function
