/* What every export provider (docs/merge-plan.md §9.9) includes after
 * gex_abi.h: the guest's registers and stack as an i386 callee sees them,
 * the return a provider makes, and waiting on a guest word.
 *
 * A provider body is an F_body (gex_body_t): it runs in place of the guest
 * function at an export's address, reached through the function table like
 * any guest function. Every caller reaches it through the table, so the
 * whole guest state is in the context on entry; a provider reads what the
 * calling convention passes, writes EAX (EDX), pops the return address (and
 * N bytes for thiscall/stdcall), and sets PC. It returns 1 when that return
 * is the one the caller predicted (`ret`), else 0 with everything written
 * back, as any F_body does. */
#ifndef GEX_PROVIDER_H
#define GEX_PROVIDER_H

/* Marks `fn` as providing `export` of the library the provider source is
 * for; the host binds it at that export's guest address. Taking the
 * function's address puts it in the module's element segment; the host reads
 * these lines from the source and binds each slot by its function's name. */
#define GEX_PROVIDE(fn, export) GEX_PROVIDE_(fn, __COUNTER__)
#define GEX_PROVIDE_(fn, n) GEX_PROVIDE__(fn, n)
#define GEX_PROVIDE__(fn, n) \
    __attribute__((export_name("gex_provide_" #n))) gex_body_t gex_provide_##n(void) { return fn; }

#define GEX_PROVIDER(name) int name(int ctx, int32_t depth, uint32_t need, uint64_t ret)

/* i386 registers in the context (64-bit slots, upper half zero). */
#define R_EAX 0
#define R_ECX 1
#define R_EDX 2
#define R_EBX 3
#define R_ESP 4
GEX_INLINE uint32_t gp_reg(int ctx, unsigned r) { return (uint32_t)GEX_C64(GEX_CTX_XN(r)); }
GEX_INLINE void gp_set(int ctx, unsigned r, uint32_t v) { GEX_C64(GEX_CTX_XN(r)) = v; }
/* A 32-bit guest word (guest VA == linear-memory offset). */
#define GW(a) (*(gex_u32 *)GEX_P((uint32_t)(a)))
#define GA(a) ((gex_a32 *)GEX_P((uint32_t)(a)))
/* Stack argument `i` (0-based, after the return address). */
GEX_INLINE uint32_t gp_arg(int ctx, unsigned i) { return GW(gp_reg(ctx, R_ESP) + 4 + 4 * i); }

/* Return to the guest: pop the return address and `pop` bytes of arguments. */
GEX_INLINE int gp_return(int ctx, uint64_t ret, unsigned pop) {
    uint32_t sp = gp_reg(ctx, R_ESP);
    uint32_t ra = GW(sp);
    gp_set(ctx, R_ESP, sp + 4 + pop);
    GEX_C64(GEX_CTX_PC) = ra;
    return ra == ret;
}
GEX_INLINE int gp_return_eax(int ctx, uint64_t ret, unsigned pop, uint32_t eax) {
    gp_set(ctx, R_EAX, eax);
    return gp_return(ctx, ret, pop);
}

/* This guest thread, never 0. */
GEX_INLINE uint32_t gp_self(int ctx) { return (uint32_t)GEX_C64(GEX_CTX_TID) + 1u; }

/* Wait while the word at `a` holds `v`, at most `ms` milliseconds (wasm
 * atomic wait on the shared guest memory); a store plus gp_wake ends it. */
GEX_INLINE void gp_wait(uint32_t a, uint32_t v, uint32_t ms) {
    (void)__builtin_wasm_memory_atomic_wait32((int *)GEX_P(a), (int)v, (int64_t)ms * 1000000);
}
GEX_INLINE void gp_wake(uint32_t a, uint32_t count) {
    (void)__builtin_wasm_memory_atomic_notify((int *)GEX_P(a), count);
}
/* Block this thread for `ms` (a wait nothing wakes: a word only it uses,
 * in its context's scratch). */
GEX_INLINE void gp_sleep(int ctx, uint32_t ms) {
    uint32_t w = (uint32_t)ctx + GEX_CTX_SCRATCH;
    gp_wait(w, GW(w), ms);
}
/* Inside a wait loop, before the provider has changed any guest state: a
 * safepoint, which may stop the thread (exit, a signal) and resume it at
 * the provider's own entry, `pc`. */
GEX_INLINE void gp_poll(int ctx, uint64_t pc) { gex_safepoint(ctx, (int64_t)pc); }
/* The address this body was entered at (every call through the table sets it). */
#define GP_SELF_PC() GEX_C64(GEX_CTX_LAST_TARGET)

#endif
