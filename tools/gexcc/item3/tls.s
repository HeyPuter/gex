# The calling guest thread's context and call depth, for calls back into
# guest code from anywhere in the library (rt.c a64_call_guest): wasm
# globals, which each Worker's instance has its own of (the module's data
# is every thread's). wasm64.
	.globaltype	a64_ctx_g, i32
a64_ctx_g:
	.globaltype	a64_depth_g, i32
a64_depth_g:

	.globl	a64_ctx
	.type	a64_ctx,@function
a64_ctx:
	.functype	a64_ctx () -> (i32)
	global.get	a64_ctx_g
	end_function

	.globl	a64_depth
	.type	a64_depth,@function
a64_depth:
	.functype	a64_depth () -> (i32)
	global.get	a64_depth_g
	end_function

	.globl	a64_set
	.type	a64_set,@function
a64_set:
	.functype	a64_set (i32, i32) -> ()
	local.get	0
	global.set	a64_ctx_g
	local.get	1
	global.set	a64_depth_g
	end_function
