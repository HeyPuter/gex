# gexcc runtime: the safepoint countdown GexPoll's loop polls decrement
# (poll/GexPoll.cpp). A module global, so each thread (instance) has its
# own. It starts at 0: a thread's first loop polls once and re-arms it.
	.globaltype __gex_poll_counter, i32
	.globl __gex_poll_counter
__gex_poll_counter:

	.globl __gex_poll_rearm
	.type __gex_poll_rearm,@function
__gex_poll_rearm:
	.functype __gex_poll_rearm (i32) -> ()
	local.get 0
	global.set __gex_poll_counter
	end_function
