/* gexcc runtime: i386 code in a wasm32 program (-fgex-guest-i386).
 *
 * A program linked with the flag may write i386 code into its own memory
 * (a JIT's code space), call it, and be called back from it: gex
 * translates that code as it translates any i386 program's, in the same
 * address space (a guest address is a linear-memory offset, so pointers
 * mean the same on both sides). The calls go through gex's function table,
 * as native Wine's (gexrt) do:
 *
 * - gex_i386_call: the program calls i386 code. The arguments are pushed
 *   as stack words below the program's own stack, with a return address no
 *   code has (the last word of gex's trampoline page), and the guest runs
 *   from the function table until it returns there.
 * - an entry (GEX_I386_ENTRY): i386 code calls the program. A body bound
 *   at a guest address (gex_i386_bind) runs when the guest calls or jumps
 *   there. The program's code runs on the guest thread's stack, below the
 *   guest's stack pointer, so the two sides share one stack and nest to
 *   any depth.
 *
 * Calls from i386 code into the program are cdecl or stdcall as the body
 * says (gex_i386_arg reads the stack words, gex_i386_return pops). */
#include <stdint.h>

#include "gex/i386.h"
#include "gex_abi.h"
#include "provider.h"

#define HIDDEN __attribute__((visibility("hidden")))

/* The return address of a call from the program into i386 code: the last
 * word of gex's trampoline page (int3 filler; gex-kernel win::tramp
 * NATIVE_RETURN_PAD), which tier-1's call-out watches. */
#define RETURN_PAD 0x3ffff1u
/* The `need` of a call that returns to the pad: the flags (ftab::NEED_FLAGS)
 * and how many bytes of arguments the callee may pop (ftab::NEED_ARGS). */
#define PAD_NEED(args) (((args) & ~3u) | 1u)

/* The thread's guest context (musl's, set by the thread's entry). */
unsigned long __gex_get_ctx(void);
__attribute__((import_module("gex"), import_name("i386_bind"))) int gex_host_i386_bind(int64_t addr, int32_t idx);

/* The thread's entry depth: how many calls of each side are nested. */
static _Thread_local int depth_now;

/* Switch the C stack to just below the guest's stack pointer, run `body`,
 * and switch back: the program's frames go below the guest's, which they
 * would otherwise overwrite. */
HIDDEN int gex_i386_enter_raw(int ctx, int32_t depth, uint32_t need, uint64_t ret, gex_i386_body_t body);
__asm__(
    "\t.globaltype __stack_pointer, i32\n"
    "\t.hidden gex_i386_enter_raw\n"
    "\t.globl gex_i386_enter_raw\n"
    "\t.type gex_i386_enter_raw,@function\n"
    "gex_i386_enter_raw:\n"
    "\t.functype gex_i386_enter_raw (i32, i32, i32, i64, i32) -> (i32)\n"
    "\t.local i32, i32\n"
    "\tglobal.get __stack_pointer\n"
    "\tlocal.set 5\n"
    /* the guest's ESP (the context's x(4), 64-bit slots), less a margin,
     * 16-aligned */
    "\tlocal.get 0\n"
    "\ti32.load 32\n"
    "\ti32.const 64\n"
    "\ti32.sub\n"
    "\ti32.const -16\n"
    "\ti32.and\n"
    "\tglobal.set __stack_pointer\n"
    "\tlocal.get 0\n"
    "\tlocal.get 1\n"
    "\tlocal.get 2\n"
    "\tlocal.get 3\n"
    "\tlocal.get 4\n"
    "\tcall_indirect __indirect_function_table, (i32, i32, i32, i64) -> (i32)\n"
    "\tlocal.set 6\n"
    "\tlocal.get 5\n"
    "\tglobal.set __stack_pointer\n"
    "\tlocal.get 6\n"
    "\tend_function\n");

int gex_i386_enter(int ctx, int32_t depth, uint32_t need, uint64_t ret, gex_i386_body_t body)
{
    int saved = depth_now, r;
    depth_now = depth;
    r = gex_i386_enter_raw(ctx, depth, need, ret, body);
    depth_now = saved;
    return r;
}

/* Run the guest from the context until it reaches `stop`, as gex's dispatch
 * loop does: a body that returns 0 has written where to go on. */
static void run_until(int ctx, uint32_t stop, uint32_t need)
{
    int depth = depth_now + 1;
    for (;;) {
        uint32_t pc = (uint32_t)GEX_C64(GEX_CTX_PC);
        if (pc == stop)
            return;
        uint32_t esp = gp_reg(ctx, R_ESP);
        GEX_C64(GEX_CTX_LAST_TARGET) = pc;
        if (GEX_FTAB(gex_ftab_idx(pc))(ctx, depth, need, stop))
            return;
        /* left where it was entered, the stack as it was (its nest full):
         * go on in a new nest from here */
        if ((uint32_t)GEX_C64(GEX_CTX_PC) == pc && gp_reg(ctx, R_ESP) == esp)
            depth = 0;
    }
}

uint64_t gex_i386_call(uint32_t target, const uint32_t *words, unsigned n)
{
    int ctx = (int)__gex_get_ctx();
    uint32_t sp0 = gp_reg(ctx, R_ESP), sp, need, i;
    /* this frame's bottom (wasm: the C stack pointer here) */
    uint32_t c_sp = (uint32_t)(uintptr_t)__builtin_frame_address(0);
    /* below everything this side has live */
    sp = ((c_sp - 64) & ~15u) - 4 * n - 4;
    GW(sp) = RETURN_PAD;
    for (i = 0; i < n; i++)
        GW(sp + 4 + 4 * i) = words[i];
    gp_set(ctx, R_ESP, sp);
    GEX_C64(GEX_CTX_PC) = target;
    GEX_C64(GEX_CTX_LAST_TARGET) = target;
    need = PAD_NEED(4 * n);
    if (!GEX_FTAB(gex_ftab_idx(target))(ctx, depth_now + 1, need, RETURN_PAD))
        run_until(ctx, RETURN_PAD, need);
    gp_set(ctx, R_ESP, sp0);
    return ((uint64_t)gp_reg(ctx, R_EDX) << 32) | gp_reg(ctx, R_EAX);
}

/* ST0, popped (an extended value narrowed to a double). */
static double x87_pop(int ctx)
{
    uint32_t top = GEX_C32(GEX_CTX_X87_TOP) & 7u, se;
    uint64_t sig = GEX_C64(GEX_CTX_X87 + 16u * top), sign;
    int32_t e, de;
    se = GEX_C16(GEX_CTX_X87 + 16u * top + 8u);
    GEX_C32(GEX_CTX_X87_TAGS) &= ~(1u << top);
    GEX_C32(GEX_CTX_X87_TOP) = (top + 1u) & 7u;
    sign = (uint64_t)(se >> 15) << 63;
    e = (int32_t)(se & 0x7fff);
    if (e == 0 && sig == 0)
        return gex_f64(sign);
    if (e == 0x7fff)
        return gex_f64(sign | 0x7ff0000000000000ull | ((sig << 1) >> 12));
    de = e - 16383 + 1023;
    if (de >= 0x7ff)
        return gex_f64(sign | 0x7ff0000000000000ull);
    if (de <= 0)
        return gex_f64(12 - de >= 64 ? sign : sign | (sig >> (12 - de)));
    return gex_f64(sign | ((uint64_t)de << 52) | ((sig << 1) >> 12));
}

double gex_i386_call_fp(uint32_t target, const uint32_t *words, unsigned n)
{
    (void)gex_i386_call(target, words, n);
    return x87_pop((int)__gex_get_ctx());
}

int gex_i386_bind(uint32_t addr, gex_i386_body_t body)
{
    return gex_host_i386_bind(addr, (int32_t)(uintptr_t)body);
}

uint32_t gex_i386_arg(int ctx, unsigned i) { return gp_arg(ctx, i); }

int gex_i386_return(int ctx, uint64_t ret, unsigned pop, uint64_t edx_eax)
{
    gp_set(ctx, R_EDX, (uint32_t)(edx_eax >> 32));
    return gp_return_eax(ctx, ret, pop, (uint32_t)edx_eax);
}

int gex_i386_return_fp(int ctx, uint64_t ret, unsigned pop, double v)
{
    gp_push_st0(ctx, v);
    return gp_return(ctx, ret, pop);
}
