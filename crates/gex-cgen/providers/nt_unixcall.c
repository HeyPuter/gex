/* PROVIDES [nt-trampolines] i386
 *
 * The i386 unix-call dispatcher (gex_kernel::win::tramp::UNIX_CALL_32,
 * what ntdll's __wine_unix_call_dispatcher points at), as the trampoline
 * makes it: the UNIX_CALL syscall (`mov eax, UNIX_CALL; call 1f; ret 16;
 * 1: int 0x80; ret`), in a unit instead of on tier-1.
 *
 * stdcall (handle: u64, code, params), 16 bytes popped; returns NTSTATUS. */

/* gex_kernel::win::TRAMPOLINES and the i386 unix-call trampoline in it. */
#define TR 0x3f0000u
#define TR_RET16 (TR + 0x6au)  /* the `ret 16` the inner call returns to */
#define TR_INT80 (TR + 0x6du)  /* `int 0x80` */
#define TR_RET (TR + 0x6fu)    /* the `ret` after it */
#define UNIX_CALL_NR 0x7f000001u /* gex_kernel::win::nr::UNIX_CALL */

static GEX_PROVIDER(p_unix_call) {
    (void)need;
    /* The trampoline: its inner call's return address pushed, the syscall
     * made at its `int 0x80` (a call that does not come straight back
     * leaves from inside gex_syscall, the guest at TR_RET with this very
     * stack), then its `ret` and `ret 16`. */
    (void)depth;
    uint32_t sp = gp_reg(ctx, R_ESP) - 4u;
    GW(sp) = TR_RET16;
    gp_set(ctx, R_ESP, sp);
    gp_set(ctx, R_EAX, UNIX_CALL_NR);
    GEX_C64(GEX_CTX_PC) = TR_RET;
    gex_syscall(ctx, (int64_t)TR_INT80);
    gp_set(ctx, R_ESP, gp_reg(ctx, R_ESP) + 4u);
    return gp_return(ctx, ret, 16);
}

GEX_PROVIDE(p_unix_call, "unix_call_32")
