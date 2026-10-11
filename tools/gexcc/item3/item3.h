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
#define LAST_TARGET 44 /* gex_abi::ctx::LAST_TARGET / 8 */
/* Integer argument i: x0-x7, then the stack. */
static inline uint64_t A64_ARG(int ctx, unsigned i)
{
	return i < 8 ? A64_X(ctx, i) : *(volatile uint64_t *)(uintptr_t)(A64_X(ctx, A64_SP) + 8 * (i - 8));
}

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

/* The offset of v0 in the context (gex_abi::ctx::V; 16 bytes a register). */
uint64_t a64_ctx_v(void);
#define A64_V(ctx, n) (((volatile uint64_t *)(uintptr_t)((uint32_t)(ctx) + a64_ctx_v()))[2 * (n)])
/* Floating-point argument `i` (v0-v7; none of these APIs has more). */
static inline double a64_varg_double(int ctx, unsigned i, unsigned *k)
{
	(void)k;
	if (i >= 8)
		__builtin_trap();
	uint64_t b = A64_V(ctx, i);
	double d;
	__builtin_memcpy(&d, &b, 8);
	return d;
}
static inline float a64_varg_float(int ctx, unsigned i, unsigned *k)
{
	(void)k;
	if (i >= 8)
		__builtin_trap();
	uint32_t b = (uint32_t)A64_V(ctx, i);
	float f;
	__builtin_memcpy(&f, &b, 4);
	return f;
}
/* Return with a floating-point result in v0 (x0 as the guest left it). */
static inline int a64_ret_v(int ctx, uint64_t ret, double d)
{
	uint64_t b;
	__builtin_memcpy(&b, &d, 8);
	A64_V(ctx, 0) = b;
	(((volatile uint64_t *)(uintptr_t)((uint32_t)(ctx) + a64_ctx_v()))[1]) = 0;
	uint64_t ra = A64_X(ctx, 30);
	A64_X(ctx, A64_PC) = ra;
	return ra == ret;
}

/* The calling thread's context and depth (tls.s), set on every entry. */
int a64_ctx(void);
int a64_depth(void);
void a64_set(int ctx, int depth);
#ifdef A64_MUSL
/* With the LP64 libc (sysroot.sh): its syscalls go through this context,
 * and a guest thread's first entry gives it libc's per-thread state. */
void __gex_set_ctx(unsigned long);
unsigned long __gex_get_tp(void);
void __gex_provider_attach(void);
#define a64_enter(ctx) a64_enter_(ctx, depth)
static inline void a64_enter_(int ctx, int depth)
{
	__gex_set_ctx((uint32_t)ctx);
	if (__builtin_expect(!__gex_get_tp(), 0))
		__gex_provider_attach();
	a64_set(ctx, depth);
}
#else
#define a64_enter(ctx) a64_set(ctx, depth)
#endif
GEXP(__gex_thread_start);

/* Call the guest function at `fn` (AAPCS64, integer arguments; at most 8)
 * from inside the library, on the calling guest thread: its x0. */
uint64_t a64_call_guest(uint64_t fn, unsigned n, const uint64_t *args);

/* The export the last adapter entered (gen.py: its table position + 1). */
extern volatile int a64_last_export;

/* Whether `p` is a block of rt.c's heap. */
int gexp_heap_owns(const void *p);

GEXP(gexp__missing);
GEXP(gexp__noop);
#endif
