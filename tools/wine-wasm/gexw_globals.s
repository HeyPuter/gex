# Per-thread state of a native Wine module under gex: wasm globals of the
# module instance (gex instantiates a module once per thread). Written in
# assembly because clang cannot address an address_space(1) global in
# position-independent code. Small accessors: V8 inlines them.
#
#   gexw_icall_target  the guest address an ICALL exit stub is to call
#   gexw_ctx           the guest context the innermost entry adapter got
#   gexw_depth         and its call depth

	.globaltype gexw_icall_target_g, i32
gexw_icall_target_g:
	.globaltype gexw_ctx_g, i32
gexw_ctx_g:
	.globaltype gexw_depth_g, i32
gexw_depth_g:

	.macro accessors name
	.globl gexw_get_\name
	.type gexw_get_\name,@function
gexw_get_\name:
	.functype gexw_get_\name () -> (i32)
	global.get gexw_\name\()_g
	end_function
	.globl gexw_set_\name
	.type gexw_set_\name,@function
gexw_set_\name:
	.functype gexw_set_\name (i32) -> ()
	local.get 0
	global.set gexw_\name\()_g
	end_function
	.endm

	accessors icall_target
	accessors ctx
	accessors depth
