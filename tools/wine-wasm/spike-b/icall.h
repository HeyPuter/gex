/* What the ICALL pass (docs/wine-native.md §3.3) makes of an indirect call
 * in native Wine code, written out by hand for W0(b).
 *
 * A function pointer below GEXW_ICALL_LIMIT is a function-table index:
 * native code, called directly. Anything else is a guest address: the call
 * goes to the exit stub for the call's wasm signature, which finds the
 * address in a per-thread wasm global. One call_indirect either way (the
 * select form measured in W0(c)).
 *
 * The pass keys exit stubs on the call's wasm signature; here C11 _Generic
 * on the pointer's type stands in for that. */
#ifndef GEXW_ICALL_H
#define GEXW_ICALL_H
#include <stdint.h>

#define GEXW_ICALL_LIMIT 0x400000u

/* Per module instance, so per thread (tools/wine-wasm/gexw_globals.s): the
 * guest target of the exit stub being called, and the guest context and
 * call depth the innermost entry adapter was given. */
uint32_t gexw_get_icall_target(void);
void gexw_set_icall_target(uint32_t);
int32_t gexw_get_ctx(void);
void gexw_set_ctx(int32_t);
int32_t gexw_get_depth(void);
void gexw_set_depth(int32_t);

int32_t gexw_exit_i_pp(const void *, const void *);
int32_t gexw_exit_i_i(int32_t);

#define GEXW_EXIT_STUB(fp) _Generic((fp), \
    int (*)(const void *, const void *): gexw_exit_i_pp, \
    int (__stdcall *)(int): gexw_exit_i_i, \
    double (*)(double, int64_t): gexw_exit_i_i)

#ifdef GEXW_ICALL_PASS
/* The pass (tools/wine-wasm/icall) rewrites every indirect call itself. */
#define ICALL(fp) (fp)
#else
#define ICALL(fp) \
    ((uintptr_t)(fp) < GEXW_ICALL_LIMIT ? (fp) \
     : (gexw_set_icall_target((uint32_t)(uintptr_t)(fp)), (__typeof__(fp))GEXW_EXIT_STUB(fp)))
#endif

#endif
