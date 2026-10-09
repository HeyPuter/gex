/* An LP64 wasm library under an aarch64 guest program (docs/gexcc.md §7.8):
 * the guest's registers, read and written by an export's adapter, a gex
 * F_body bound at the arm64 export's address (shim cgen/libprov.rs). */
#ifndef ITEM3_H
#define ITEM3_H
#include <stddef.h>
#include <stdint.h>

/* The guest thread's context: x0-x30 8 bytes each, SP slot 31, PC 32
 * (gex_abi::ctx, the aarch64 build); a 32-bit address in the guest's memory,
 * which the lowered module shares. */
#define A64_X(ctx, n) (((volatile uint64_t *)(uintptr_t)(uint32_t)(ctx))[n])
#define A64_SP 31
#define A64_PC 32
#define A64_ARG(ctx, i) ((i) < 8 ? A64_X(ctx, i) : *(volatile uint64_t *)(uintptr_t)(A64_X(ctx, A64_SP) + 8 * ((i) - 8)))

#define GEXP(name) int name(int ctx, int32_t depth, uint32_t need, uint64_t ret)

/* Return to the caller (x30) with x0 = v: 1 when that is where the caller
 * predicted (`ret`), as every F_body reports. */
static inline int a64_ret(int ctx, uint64_t ret, uint64_t v)
{
	A64_X(ctx, 0) = v;
	uint64_t ra = A64_X(ctx, 30);
	A64_X(ctx, A64_PC) = ra;
	return ra == ret;
}

GEXP(gexp__missing);
GEXP(gexp__noop);
#endif
