/* PROVIDES [nt-trampolines] i386
 *
 * The i386 unix-call dispatcher (gex_kernel::nt::boot::tramp::UNIX_CALL_32,
 * what ntdll's __wine_unix_call_dispatcher points at), with the hottest
 * OpenGL calls written straight into the GL command ring. Wine's PE
 * opengl32 makes one unix call per GL function; Noita makes about 3,900 a
 * frame, 81% of them the seven below, and each was a trip through the NT
 * layer (the syscall, the unix-call dispatch, opengl32's Unix side) to
 * append 12 to 16 bytes to the ring. Here they are those bytes: the same
 * command the layer writes for them (gex_kernel::nt::opengl's `hot_one`).
 *
 * The ring is the layer's: it publishes the ring's address at GL_FAST_RING
 * once the device is open (0 before, and with GEX_NT_GL_FAST=0), and the
 * tail at GL_FAST_TAIL, whose top bit is the ring's lock (the layer holds it
 * while it writes a command or submits). Anything else -- another library,
 * another GL function, no ring yet, the lock held, a full ring -- is the
 * trampoline's own work: the UNIX_CALL syscall, made as the trampoline
 * makes it (`mov eax, UNIX_CALL; call 1f; ret 16; 1: int 0x80; ret`).
 *
 * stdcall (handle: u64, code, params), 16 bytes popped; returns NTSTATUS. */

/* gex_kernel::nt::TRAMPOLINES and the i386 unix-call trampoline in it. */
#define TR 0x3f0000u
#define TR_RET16 (TR + 0x6au)  /* the `ret 16` the inner call returns to */
#define TR_INT80 (TR + 0x6du)  /* `int 0x80` */
#define TR_RET (TR + 0x6fu)    /* the `ret` after it */
#define UNIX_CALL_NR 0x7f000001u /* gex_kernel::nt::gex_nr::UNIX_CALL */

/* gex_kernel::nt::opengl: the fast path's words and the ring's shape. */
#define GL_FAST_RING 0x7ffe0fe0u
#define GL_FAST_TAIL 0x7ffe0fe4u
#define TAIL_BUSY 0x80000000u
#define RING_HEADER 32u
#define CMD_CAPACITY (8u << 20)
/* opengl32's unixlib handle ((3 << 16) | 1, gex_kernel::nt::unixcall). */
#define GL_HANDLE 0x30001u

/* opengl32's unix-call codes (`enum unix_funcs`, gex_kernel::nt::opengl::
 * table::FUNCS) and gex-gl's opcodes (gex_gl::proto::op). Checked against
 * both by gex-kernel's test `nt_unixcall_provider_constants`. */
#define FN_glBindTexture 14u
#define FN_glDisable 72u
#define FN_glEnable 81u
#define FN_glActiveTexture 350u
#define FN_glUniform1i 2524u
#define FN_glUniform2f 2542u
#define FN_glUseProgram 2645u
#define OP_ENABLE 19u
#define OP_DISABLE 20u
#define OP_USE_PROGRAM 55u
#define OP_UNIFORM_1I 64u
#define OP_UNIFORM_2F 66u
#define OP_BIND_TEXTURE 98u
#define OP_ACTIVE_TEXTURE 99u

/* Append one command of `n` argument words (1 to 3) taken from the params
 * struct (after its TEB pointer). 1 when it is in the ring. */
GEX_INLINE int gl_put(uint32_t op, uint32_t params, uint32_t n) {
    uint32_t ring = __atomic_load_n(GA(GL_FAST_RING), __ATOMIC_ACQUIRE);
    if (!ring) {
        return 0;
    }
    uint32_t len = 8u + 4u * n;
    uint32_t t = __atomic_load_n(GA(GL_FAST_TAIL), __ATOMIC_ACQUIRE);
    if ((t & TAIL_BUSY) || t + len > CMD_CAPACITY) {
        return 0;
    }
    if (!__atomic_compare_exchange_n(GA(GL_FAST_TAIL), &t, t | TAIL_BUSY, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
        return 0;
    }
    uint32_t at = ring + RING_HEADER + t;
    GW(at) = op;
    GW(at + 4u) = len;
    for (uint32_t i = 0; i < n; ++i) {
        GW(at + 8u + 4u * i) = GW(params + 4u + 4u * i);
    }
    __atomic_store_n(GA(GL_FAST_TAIL), t + len, __ATOMIC_RELEASE);
    return 1;
}

static GEX_PROVIDER(p_unix_call) {
    (void)need;
    if (gp_arg(ctx, 0) == GL_HANDLE && gp_arg(ctx, 1) == 0) {
        uint32_t code = gp_arg(ctx, 2), params = gp_arg(ctx, 3), op = 0, n = 0;
        switch (code) {
        case FN_glEnable: op = OP_ENABLE, n = 1; break;
        case FN_glDisable: op = OP_DISABLE, n = 1; break;
        case FN_glActiveTexture: op = OP_ACTIVE_TEXTURE, n = 1; break;
        case FN_glUseProgram: op = OP_USE_PROGRAM, n = 1; break;
        case FN_glBindTexture: op = OP_BIND_TEXTURE, n = 2; break;
        case FN_glUniform1i: op = OP_UNIFORM_1I, n = 2; break;
        case FN_glUniform2f: op = OP_UNIFORM_2F, n = 3; break;
        default: break;
        }
        if (n && params >= 0x10000u && gl_put(op, params, n)) {
            return gp_return_eax(ctx, ret, 16, 0);
        }
    }
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
