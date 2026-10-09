/* gexcc: setjmp.h over musl's. clang's wasm setjmp/longjmp lowering
 * (-mllvm -wasm-enable-sjlj) recognises calls to `setjmp` only; a call to
 * sigsetjmp or _setjmp is an ordinary call that returns 0 once, and a
 * longjmp to it is an uncaught exception (bash's error recovery, every
 * program that siglongjmps out of a signal handler). Here both are setjmp
 * itself, with sigsetjmp's mask saved before and restored on the second
 * return (rt/sigsetjmp.c), as musl's assembly does on other targets. */
#include_next <setjmp.h>

#ifndef __GEXCC_SETJMP_H
#define __GEXCC_SETJMP_H
#ifdef __cplusplus
extern "C" {
#endif
void __gexcc_sigsetjmp_save(struct __jmp_buf_tag *, int);
int __gexcc_sigsetjmp_post(struct __jmp_buf_tag *, int);
#ifdef __cplusplus
}
#endif
#define sigsetjmp(b, s) __gexcc_sigsetjmp_post((b), (__gexcc_sigsetjmp_save((b), (s)), setjmp(b)))
#define _setjmp(b) setjmp(b)
#endif
