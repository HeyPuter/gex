/* The run-time half of native Wine modules under gex (docs/wine-native.md
 * §3.3): calling guest (x86) code from native code. The ICALL pass's exit
 * stubs lay a call's arguments out as i386 stack words and call these.
 *
 * Compiled into every native module with gex_abi.h and provider.h (the
 * export providers' view of the guest's registers). */
#include "gex_abi.h"
#include "provider.h"

__attribute__((import_module("gex"), import_name("provider_ctx"))) int gex_provider_ctx(void);

/* Per-thread state, wasm globals (gexw_globals.s). */
int32_t gexw_get_ctx(void);
int32_t gexw_get_depth(void);

/* Call the guest function at `target` with `n` stack words: push them and a
 * return address of our own, run it through the function table, and give
 * back EDX:EAX. Whatever the callee popped (cdecl or stdcall), the guest
 * stack is put back as it was. */
uint64_t __gexw_guest_call(uint32_t target, const uint32_t *words, uint32_t n)
{
    int ctx = gexw_get_ctx();
    if (!ctx) ctx = gex_provider_ctx();
    uint32_t sp0 = gp_reg(ctx, R_ESP);
    uint32_t sp = sp0 - 4 * n - 4;
    /* In the callee's page, so tier-1's call-out can watch for it. */
    uint32_t ra = target ^ 1u;
    GW(sp) = ra;
    for (uint32_t i = 0; i < n; i++) GW(sp + 4 + 4 * i) = words[i];
    gp_set(ctx, R_ESP, sp);
    GEX_C64(GEX_CTX_LAST_TARGET) = target;
    if (!GEX_FTAB(gex_ftab_idx(target))(ctx, gexw_get_depth() + 1, 0xffffffffu, ra))
        __builtin_trap(); /* the guest left another way: W2 unwinds here */
    gp_set(ctx, R_ESP, sp0);
    return ((uint64_t)gp_reg(ctx, R_EDX) << 32) | gp_reg(ctx, R_EAX);
}

/* The same for a function returning float or double: the result is in
 * ST0, popped here. */
double __gexw_guest_call_fp(uint32_t target, const uint32_t *words, uint32_t n)
{
    int ctx = gexw_get_ctx();
    if (!ctx) ctx = gex_provider_ctx();
    (void)__gexw_guest_call(target, words, n);
    uint32_t top = GEX_C32(GEX_CTX_X87_TOP) & 7u;
    uint64_t sig = GEX_C64(GEX_CTX_X87 + 16u * top);
    uint32_t se = GEX_C16(GEX_CTX_X87 + 16u * top + 8u);
    GEX_C32(GEX_CTX_X87_TAGS) &= ~(1u << top);
    GEX_C32(GEX_CTX_X87_TOP) = (top + 1u) & 7u;
    /* 80-bit extended to double (round toward zero; exact for anything a
     * double return put there). */
    uint64_t sign = (uint64_t)(se >> 15) << 63;
    int32_t e = (int32_t)(se & 0x7fff);
    if (e == 0 && sig == 0) return gex_f64(sign);
    if (e == 0x7fff) return gex_f64(sign | 0x7ff0000000000000ull | ((sig << 1) >> 12));
    int32_t de = e - 16383 + 1023;
    if (de >= 0x7ff) return gex_f64(sign | 0x7ff0000000000000ull);
    if (de <= 0) {
        int shift = 12 - de;
        return gex_f64(shift >= 64 ? sign : sign | (sig >> shift));
    }
    return gex_f64(sign | ((uint64_t)de << 52) | ((sig << 1) >> 12));
}
