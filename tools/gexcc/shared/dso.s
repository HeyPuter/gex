# gexcc (shared): this module's own __tls_base setter, which libc calls
# for a thread whose TLS area already holds the module's block (a fork's
# child): src/env/wasm32/__gex_dso.c in gex's musl.
	.globaltype __tls_base, i32
	.globl __gex_set_tls_base
	.type __gex_set_tls_base,@function
__gex_set_tls_base:
	.functype __gex_set_tls_base (i32) -> ()
	local.get 0
	global.set __tls_base
	end_function
