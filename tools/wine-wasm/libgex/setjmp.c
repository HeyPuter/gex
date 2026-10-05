/* libgex: setjmp and longjmp for wasm32 programs under gex.
 *
 * clang lowers setjmp/longjmp (-mllvm -wasm-enable-sjlj, part of gex's
 * wasm32 flags, tools/wine-wasm/flags.sh) onto wasm exception handling;
 * this is the runtime that lowering calls (wasi-libc's layout) and the tag
 * it throws. A setjmp call that was not lowered (code compiled without the
 * flag) cannot work and returns 0; longjmp always throws.
 *
 * Kept out of musl (docs/wine-native.md §3.11.2): musl's generic
 * setjmp.c, longjmp.c and sigsetjmp.c are empty, so libc defines none of
 * these and wcc links this library with every program. siglongjmp is
 * musl's (it calls longjmp). */
#include <setjmp.h>
#include <stdint.h>

__asm__(
	"\t.tagtype __c_longjmp i32\n"
	"\t.globl __c_longjmp\n"
	"__c_longjmp:\n"
);

struct jmp_buf_impl
{
	void *func_invocation_id;
	uint32_t label;
	struct { void *env; int val; } arg;
};

void __wasm_setjmp(void *env, uint32_t label, void *func_invocation_id)
{
	struct jmp_buf_impl *jb = env;
	jb->func_invocation_id = func_invocation_id;
	jb->label = label;
}

uint32_t __wasm_setjmp_test(void *env, void *func_invocation_id)
{
	struct jmp_buf_impl *jb = env;
	return jb->func_invocation_id == func_invocation_id ? jb->label : 0;
}

_Noreturn void __wasm_longjmp(void *env, int val)
{
	struct jmp_buf_impl *jb = env;
	jb->arg.env = env;
	jb->arg.val = val ? val : 1;
	__builtin_wasm_throw(1, &jb->arg);
}

int setjmp(jmp_buf env) { return 0; }
int _setjmp(jmp_buf env) { return 0; }
int sigsetjmp(sigjmp_buf env, int savemask) { return 0; }
_Noreturn void longjmp(jmp_buf env, int val) { __wasm_longjmp(env, val); }
_Noreturn void _longjmp(jmp_buf env, int val) { __wasm_longjmp(env, val); }
