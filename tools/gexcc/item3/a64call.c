/* Calls from an item-3 library into the aarch64 guest's code (a caller's
 * allocator, a signal handler): on the calling guest thread, returning to
 * the library's code page, the dispatch loop of gexprov64.c. */
#include "item3.h"

volatile int a64_last_export;

__attribute__((import_module("gex"), import_name("provider_code_page"))) uint64_t gexp_host_code_page(void);
__attribute__((import_module("gex"), import_name("provider_abi"))) uint64_t gexp_host_abi(unsigned which);
__attribute__((import_module("gex"), import_name("provider_guest_call"))) void gexp_host_guest_call(int delta);

typedef int (*body_t)(int ctx, int32_t depth, uint32_t need, uint64_t ret);

static uint64_t code_page, fcache, fcache_bits;

/* gex's function cache: a guest address's table index, or 0 (the resolve
 * slot, which finds or translates it). */
static uint32_t ftab_idx(uint64_t t)
{
	if (__builtin_expect(!fcache, 0)) {
		fcache = gexp_host_abi(0);
		fcache_bits = gexp_host_abi(1);
	}
	uint64_t slot = (uint32_t)((uint32_t)t * 0x9e3779b1u) >> (32 - fcache_bits);
	uint64_t e = __atomic_load_n((uint64_t *)(uintptr_t)(fcache + 8 * slot), __ATOMIC_RELAXED);
	return (e & 0xffffffffull) == (t & 0xffffffffull) ? (uint32_t)(e >> 32) : 0;
}

static uint64_t ctx_v;

/* The general form: integer arguments x[0..nx) in x0-x7, floating-point ones
 * (their bits) v[0..nv) in v0-v7, `nstk` 8-byte words on the stack, x8 (an
 * indirect result's address), as AAPCS64 lays out a call. Returns x0; v0's
 * low 64 bits go to *v0 when asked. */
uint64_t a64_call_guest_regs(uint64_t fn, const uint64_t *x, unsigned nx, const uint64_t *v, unsigned nv,
                             const uint64_t *stk, unsigned nstk, uint64_t x8, uint64_t *v0)
{
	int ctx = a64_ctx(), depth = a64_depth();
	if (__builtin_expect(!code_page, 0))
		code_page = gexp_host_code_page();
	if (__builtin_expect(!ctx_v, 0))
		ctx_v = gexp_host_abi(2);
	/* The callee keeps SP and x19-x29. What the adapter still needs of the
	 * guest's own state (its return address, its stack) is put back after. */
	uint64_t lr0 = A64_X(ctx, 30), sp0 = A64_X(ctx, A64_SP);
	uint64_t ra = code_page + 0x100 + 4 * ((uint32_t)depth % 0x380);
	for (unsigned i = 0; i < nx && i < 8; i++)
		A64_X(ctx, i) = x[i];
	volatile uint64_t *vr = (volatile uint64_t *)(uintptr_t)((uint32_t)ctx + ctx_v);
	for (unsigned i = 0; i < nv && i < 8; i++) {
		vr[2 * i] = v[i];
		vr[2 * i + 1] = 0;
	}
	A64_X(ctx, 8) = x8;
	A64_X(ctx, 30) = ra;
	uint64_t sp = (sp0 - 8 * (uint64_t)nstk) & ~15ull;
	for (unsigned i = 0; i < nstk; i++)
		*(volatile uint64_t *)(uintptr_t)(sp + 8 * i) = stk[i];
	A64_X(ctx, A64_SP) = sp;
	gexp_host_guest_call(1);
	uint64_t pc = fn;
	for (;;) {
		A64_X(ctx, LAST_TARGET) = pc;
		body_t f = (body_t)(uintptr_t)ftab_idx(pc);
		if (f(ctx, depth + 1, 0, ra))
			break;
		pc = A64_X(ctx, A64_PC);
		if (pc == ra)
			break;
	}
	gexp_host_guest_call(-1);
	a64_set(ctx, depth);
	uint64_t x0 = A64_X(ctx, 0);
	if (v0)
		*v0 = vr[0];
	A64_X(ctx, 30) = lr0;
	A64_X(ctx, A64_SP) = sp0;
	return x0;
}

uint64_t a64_call_guest(uint64_t fn, unsigned n, const uint64_t *args)
{
	return a64_call_guest_regs(fn, args, n, 0, 0, 0, 0, 0, 0);
}

uint64_t a64_ctx_v(void)
{
	if (__builtin_expect(!ctx_v, 0))
		ctx_v = gexp_host_abi(2);
	return ctx_v;
}

/* ---- the GexA64Icall pass's runtime (a64icall/GexA64Icall.cpp) ---- */

/* The guest function an exit stub is about to call (set at the call site;
 * a per-thread wasm global, tls.s). */
void a64_icall_set(uint64_t fn);
uint64_t a64_icall_get(void);
void __a64_set_icall_target(uint64_t fn) { a64_icall_set(fn); }

uint64_t __a64_guest_call(const uint64_t *x, unsigned nx, const uint64_t *v, unsigned nv,
                          const uint64_t *stk, unsigned nstk, uint64_t x8, uint64_t *v0)
{
	return a64_call_guest_regs(a64_icall_get(), x, nx, v, nv, stk, nstk, x8, v0);
}

/* An entry thunk's side: the guest called a function of the library through
 * its address (a table index, bound as a guest address). */
void __a64_entry(int ctx, int depth) { a64_enter(ctx); }
uint64_t __a64_reg(int ctx, unsigned i) { return A64_X(ctx, i); }
uint64_t __a64_vreg(int ctx, unsigned i)
{
	if (__builtin_expect(!ctx_v, 0))
		ctx_v = gexp_host_abi(2);
	return *(volatile uint64_t *)(uintptr_t)((uint32_t)ctx + ctx_v + 16 * i);
}
uint64_t __a64_stack(int ctx, unsigned off) { return *(volatile uint64_t *)(uintptr_t)(A64_X(ctx, A64_SP) + off); }
int __a64_ret(int ctx, uint64_t ret, uint64_t x0) { return a64_ret(ctx, ret, x0); }
int __a64_ret_v(int ctx, uint64_t ret, uint64_t bits)
{
	if (__builtin_expect(!ctx_v, 0))
		ctx_v = gexp_host_abi(2);
	volatile uint64_t *vr = (volatile uint64_t *)(uintptr_t)((uint32_t)ctx + ctx_v);
	vr[0] = bits;
	vr[1] = 0;
	return a64_ret(ctx, ret, A64_X(ctx, 0));
}

