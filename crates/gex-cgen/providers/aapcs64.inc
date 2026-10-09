/* AAPCS64 (aarch64 Linux), ahead of an x86-64 provider's body: the `gp64_`
 * helpers it calls mean aarch64's convention here. Integer and pointer
 * arguments come in x0-x7, then the stack at SP (8 bytes each); the result
 * goes in x0 (a floating-point one in v0, the same context slot as xmm0);
 * the return is to x30, and SP is the caller's throughout. The SysV
 * register names are gone, so a body that reaches for one does not build. */
GEX_INLINE uint64_t a64_arg(int ctx, unsigned i) {
    return i < 8u ? GEX_C64(GEX_CTX_XN(i)) : GW64(GEX_C64(GEX_CTX_SP) + 8u * (i - 8u));
}
GEX_INLINE int a64_return(int ctx, uint64_t ret) {
    uint64_t ra = GEX_C64(GEX_CTX_XN(30));
    GEX_C64(GEX_CTX_PC) = ra;
    return ra == ret;
}
GEX_INLINE int a64_return_x0(int ctx, uint64_t ret, uint64_t x0) {
    GEX_C64(GEX_CTX_XN(0)) = x0;
    return a64_return(ctx, ret);
}
#define gp64_arg a64_arg
#define gp64_return a64_return
#define gp64_return_rax a64_return_x0
#define gp64_call6 gp64_call6_is_sysv_only
#undef R_RAX
#undef R_RCX
#undef R_RDX
#undef R_RSP
#undef R_RSI
#undef R_RDI
#undef R_R8
#undef R_R9
