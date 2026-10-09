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

uint64_t a64_call_guest(uint64_t fn, unsigned n, const uint64_t *args)
{
	int ctx = a64_ctx(), depth = a64_depth();
	if (__builtin_expect(!code_page, 0))
		code_page = gexp_host_code_page();
	/* AAPCS64: x0-x7, the return address in x30; the callee keeps SP and
	 * x19-x29. What the adapter still needs of the guest's own state (its
	 * return address, its stack) is put back after. */
	uint64_t lr0 = A64_X(ctx, 30), sp0 = A64_X(ctx, A64_SP);
	uint64_t ra = code_page + 0x100 + 4 * ((uint32_t)depth % 0x380);
	for (unsigned i = 0; i < n && i < 8; i++)
		A64_X(ctx, i) = args[i];
	A64_X(ctx, 30) = ra;
	A64_X(ctx, A64_SP) = sp0 & ~15ull;
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
	A64_X(ctx, 30) = lr0;
	A64_X(ctx, A64_SP) = sp0;
	return x0;
}

