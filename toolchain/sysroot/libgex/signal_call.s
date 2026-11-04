# libgex: __gex_call_handler(handler, sig, info, uc, sp, rt) runs a signal
# handler on the C stack at `sp` (just below the kernel's frame): rt
# handlers are void (int, siginfo_t *, void *), the others void (int).
# (signal.c; assembly because C cannot set the stack pointer.)
	.globaltype __stack_pointer, i32
	.globl __gex_call_handler
	.type __gex_call_handler,@function
__gex_call_handler:
	.functype __gex_call_handler (i32, i32, i32, i32, i32, i32) -> ()
	.local i32
	global.get __stack_pointer
	local.set 6
	local.get 4
	global.set __stack_pointer
	local.get 5
	if
	local.get 1
	local.get 2
	local.get 3
	local.get 0
	call_indirect __indirect_function_table, (i32, i32, i32) -> ()
	else
	local.get 1
	local.get 0
	call_indirect __indirect_function_table, (i32) -> ()
	end_if
	local.get 6
	global.set __stack_pointer
	end_function
