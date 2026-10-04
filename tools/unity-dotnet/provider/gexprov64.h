/* A wasm64 stateful provider in an x86-64 guest process (docs/unity-dotnet.md):
 * the register view of the calling guest thread, entering and returning, and
 * the host imports. Built against the wasm64 musl sysroot
 * (tools/unity-dotnet/sysroot), linked into a position-independent module
 * that gex binds at the replaced library's exports (shims/native libprov).
 *
 * An export's adapter is a gex F_body named gexp_<export>:
 *
 *     GEXP_SYSV(gexp_mono_jit_init) {
 *         GEXP_RET_U64(ctx, (uint64_t)mono_jit_init((const char *)GEXP_ARG(ctx, 0)));
 *         return gexp_return(ctx, ret);
 *     }
 *
 * and appears in GEXP_TABLE, which puts it in the module's element segment
 * where the host finds it by name. */
#ifndef GEXPROV64_H
#define GEXPROV64_H
#include <stdint.h>
#include <string.h>

typedef int (*gexp_body_t)(int ctx, int32_t depth, uint32_t need, uint64_t ret);

/* The guest thread's register context: 8-byte slots in x86 encoding order
 * (rax 0, rcx 1, rdx 2, rbx 3, rsp 4, rbp 5, rsi 6, rdi 7, r8..r15), PC at
 * slot 32, and xmm registers 16 bytes each from GEXP_CTX_V. */
#define GEXP_X(ctx, n) (((volatile uint64_t *)(uintptr_t)(uint32_t)(ctx))[n])
#define GEXP_PC 32
#define GEXP_CTX_V 0x180 /* gex_abi::ctx::V (48 * 8) */
#define GEXP_XMM_D(ctx, n) (*(volatile double *)((uintptr_t)(uint32_t)(ctx) + GEXP_CTX_V + 16 * (n)))
#define GEXP_XMM_F(ctx, n) (*(volatile float *)((uintptr_t)(uint32_t)(ctx) + GEXP_CTX_V + 16 * (n)))
#define GEXP_MEM64(a) (*(volatile uint64_t *)(uintptr_t)(a))

void __gex_set_ctx(unsigned long);
unsigned long __gex_get_tp(void);
void __gex_provider_attach(void);

/* Every adapter's first act: this thread's context for libc's syscalls,
 * and libc's per-thread state on its first visit. */
static inline void gexp_enter(int ctx)
{
	__gex_set_ctx((uint32_t)ctx);
	if (__builtin_expect(!__gex_get_tp(), 0))
		__gex_provider_attach();
}

#define GEXP_SYSV(name) int name(int ctx, int32_t depth, uint32_t need, uint64_t ret)

/* SysV x86-64 integer argument `i` (0-based): rdi, rsi, rdx, rcx, r8, r9,
 * then the stack above the return address. */
static inline uint64_t gexp_arg(int ctx, unsigned i)
{
	static const unsigned char reg[6] = { 7, 6, 2, 1, 8, 9 };
	if (i < 6)
		return GEXP_X(ctx, reg[i]);
	return GEXP_MEM64(GEXP_X(ctx, 4) + 8 + 8 * (i - 6));
}
#define GEXP_ARG(ctx, i) gexp_arg(ctx, i)

/* Win64: rcx, rdx, r8, r9, then the stack above the return address and the
 * 32-byte home area. Floating-point arguments are positional (xmm0..3). */
static inline uint64_t gexp_arg_win64(int ctx, unsigned i)
{
	static const unsigned char reg[4] = { 1, 2, 8, 9 };
	if (i < 4)
		return GEXP_X(ctx, reg[i]);
	return GEXP_MEM64(GEXP_X(ctx, 4) + 8 + 8 * i);
}

#define GEXP_RET_U64(ctx, v) (GEXP_X(ctx, 0) = (uint64_t)(v))
#define GEXP_RET_F64(ctx, v) (GEXP_XMM_D(ctx, 0) = (v))

/* Return to the guest: pop the return address into PC. 1 when it is the
 * one the caller predicted (`ret`), as every F_body reports. */
static inline int gexp_return(int ctx, uint64_t ret)
{
	uint64_t sp = GEXP_X(ctx, 4);
	uint64_t ra = GEXP_MEM64(sp);
	GEXP_X(ctx, 4) = sp + 8;
	GEXP_X(ctx, GEXP_PC) = ra;
	return ra == ret;
}

/* Host imports (shims/native libprov). */
__attribute__((import_module("gex"), import_name("provider_missing"))) void gexp_host_missing(uint64_t addr);
__attribute__((import_module("gex"), import_name("provider_log"))) void gexp_host_log(const void *p, uint64_t n);
__attribute__((import_module("gex"), import_name("provider_thread_entry"))) uint64_t gexp_host_thread_entry(void);

/* Bound at every export the provider does not implement. */
#define GEXP_LAST_TARGET 44 /* gex_abi::ctx::LAST_TARGET / 8 */
#define GEXP_DEFINE_MISSING \
	GEXP_SYSV(gexp__missing) { gexp_host_missing(GEXP_X(ctx, GEXP_LAST_TARGET)); __builtin_trap(); }

/* musl's clone starts a thread at the address the host bound to
 * __gex_thread_start; a provider's is not the program's AT_ENTRY + 16. */
#define GEXP_DEFINE_THREAD_ENTRY \
	unsigned long __gex_thread_entry(void) { return gexp_host_thread_entry(); }

/* The element segment: every adapter, plus musl's thread entry. */
int __gex_thread_start(int, int, int, long long);
#define GEXP_TABLE(...) \
	__attribute__((used)) gexp_body_t const gexp_table[] = { __VA_ARGS__, gexp__missing, (gexp_body_t)__gex_thread_start }

#endif
