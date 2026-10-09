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

	.globaltype	a64_icall_g, i64
a64_icall_g:

	.globl	a64_icall_set
	.type	a64_icall_set,@function
a64_icall_set:
	.functype	a64_icall_set (i64) -> ()
	local.get	0
	global.set	a64_icall_g
	end_function

	.globl	a64_icall_get
	.type	a64_icall_get,@function
a64_icall_get:
	.functype	a64_icall_get () -> (i64)
	global.get	a64_icall_g
	end_function

	.globaltype	a64_va_name_g, i64
a64_va_name_g:
	.globaltype	a64_va_counts_g, i32
a64_va_counts_g:

	.globl	a64_va_set
	.type	a64_va_set,@function
a64_va_set:
	.functype	a64_va_set (i64, i32) -> ()
	local.get	0
	global.set	a64_va_name_g
	local.get	1
	global.set	a64_va_counts_g
	end_function

	.globl	a64_va_name
	.type	a64_va_name,@function
a64_va_name:
	.functype	a64_va_name () -> (i64)
	global.get	a64_va_name_g
	end_function

	.globl	a64_va_counts
	.type	a64_va_counts,@function
a64_va_counts:
	.functype	a64_va_counts () -> (i32)
	global.get	a64_va_counts_g
	end_function
