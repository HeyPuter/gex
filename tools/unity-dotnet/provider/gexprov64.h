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

/* The calling guest thread's context and call depth, for calls back into
 * guest code from anywhere in the provider (gexprov64.c). */
extern __thread int gexp_ctx;
extern __thread int32_t gexp_depth;

/* Crossing between guest code and the provider (gexprov64.c): which side
 * a thread is on, for stopping the guest's threads around a collection,
 * and the provider's own hooks (a runtime's GC-safe transitions). */
void gexp_from_guest(void);
void gexp_to_guest(void);
void gexp_attach_thread(void);

/* Every adapter's first act: this thread's context for libc's syscalls,
 * and libc's per-thread state on its first visit. Nested entries (guest
 * code the provider called, calling the provider again) see the same
 * context; the depth is the caller's. */
#define gexp_enter(ctx) gexp_enter_(ctx, depth)
static inline void gexp_enter_(int ctx, int32_t depth)
{
	__gex_set_ctx((uint32_t)ctx);
	/* (TLS exists only once the thread is attached.) */
	if (__builtin_expect(!__gex_get_tp(), 0)) {
		__gex_provider_attach();
		gexp_attach_thread();
	}
	gexp_ctx = ctx;
	gexp_depth = depth;
	gexp_from_guest();
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
	gexp_to_guest();
	uint64_t sp = GEXP_X(ctx, 4);
	uint64_t ra = GEXP_MEM64(sp);
	GEXP_X(ctx, 4) = sp + 8;
	GEXP_X(ctx, GEXP_PC) = ra;
	return ra == ret;
}

/* ---- calling guest code (gexprov64.c) ----
 * A call by the guest ABI to the guest function at `fn`, from inside the
 * provider, on the current guest thread: arguments in registers and on the
 * guest stack below the current guest RSP, a return address of its own, the
 * function reached through gex's function table like any call. The guest's
 * callee-saved registers and RSP are as they were afterwards. */
typedef struct {
	uint64_t ireg[6];      /* SysV: rdi rsi rdx rcx r8 r9; Win64: rcx rdx r8 r9 */
	uint64_t xmm[8];       /* low 64 bits of xmm0..7 (a float in the low 32) */
	unsigned nireg, nxmm;  /* used: SysV puts nxmm in al (varargs) */
	const uint64_t *stack; /* stack arguments, first at the lowest address */
	unsigned nstack;
	int win64;             /* the Win64 convention: 32 bytes of home space */
	/* results */
	uint64_t rax, rdx, xmm0, xmm1;
} gexp_call;

void gexp_call_guest(uint64_t fn, gexp_call *c);

/* A guest call whose callee raised (gexp_raise) instead of returning: the
 * guest's registers are restored to before the call, and the payload is
 * returned; 0 when it returned normally. */
uint64_t gexp_call_guest_catching(uint64_t fn, gexp_call *c);

/* From provider code that guest code called during gexp_call_guest_catching:
 * abandon the guest frames back to that call, which returns `payload`. Only
 * when gexp_catching() says one is active. */
_Noreturn void gexp_raise(uint64_t payload);
int gexp_catching(void);

/* Whether a function pointer is a guest address rather than one of this
 * module's own (a table index in its slots). */
int gexp_is_guest_fn(uint64_t p);

/* ---- guest-callable addresses for the provider's own functions ----
 * gexp_reverse_alloc(data) returns a guest address that guest code can call
 * (a slot in the provider's code region, bound to its `gexp__reverse`
 * body); gexp_reverse_data(addr) finds `data` again from inside that body
 * (GEXP_X(ctx, GEXP_LAST_TARGET) on entry is the address). 0 when full. */
uint64_t gexp_reverse_alloc(void *data);
void *gexp_reverse_data(uint64_t addr);

/* The arguments guest code passed to a reverse slot, gathered from its
 * registers and stack by the guest ABI, into a gexp_call (stack = the first
 * stack argument, in guest memory); results go back from its rax/rdx/xmm0/
 * xmm1 by gexp_reverse_return. */
void gexp_reverse_args(int ctx, gexp_call *c);
int gexp_reverse_return(int ctx, uint64_t ret, const gexp_call *c);

/* Host imports (shims/native libprov). */
__attribute__((import_module("gex"), import_name("provider_missing"))) void gexp_host_missing(uint64_t addr);
__attribute__((import_module("gex"), import_name("provider_log"))) void gexp_host_log(const void *p, uint64_t n);
__attribute__((import_module("gex"), import_name("provider_thread_entry"))) uint64_t gexp_host_thread_entry(void);
__attribute__((import_module("gex"), import_name("provider_stack"))) uint64_t gexp_host_stack(void);
__attribute__((import_module("gex"), import_name("provider_abi"))) uint64_t gexp_host_abi(int which);
__attribute__((import_module("gex"), import_name("provider_try"))) int gexp_host_try(void (*fn)(void *), void *arg);
__attribute__((import_module("gex"), import_name("provider_throw"))) _Noreturn void gexp_host_throw(int code);
/* gexp_host_abi(which): */
#define GEXP_ABI_FCACHE 0       /* gex_abi::FCACHE */
#define GEXP_ABI_FCACHE_BITS 1  /* gex_abi::FCACHE_BITS */
#define GEXP_ABI_CTX_V 2        /* gex_abi::ctx::V */
#define GEXP_STACK_SIZE (1ul << 20) /* shims/native libprov::STACK */

/* Bound at every export the provider does not implement. */
#define GEXP_LAST_TARGET 44 /* gex_abi::ctx::LAST_TARGET / 8 */
#define GEXP_DEFINE_MISSING \
	GEXP_SYSV(gexp__missing) { gexp_host_missing(GEXP_X(ctx, GEXP_LAST_TARGET)); __builtin_trap(); } \
	/* Bound at the replaced library's constructors and destructors (DT_INIT, \
	 * INIT_ARRAY, DT_FINI, FINI_ARRAY), which the guest's loader calls. */ \
	GEXP_SYSV(gexp__noop) { return gexp_return(ctx, ret); }

/* musl's clone starts a thread at the address the host bound to
 * __gex_thread_start; a provider's is not the program's AT_ENTRY + 16. */
#define GEXP_DEFINE_THREAD_ENTRY \
	unsigned long __gex_thread_entry(void) { return gexp_host_thread_entry(); } \
	int __gex_thread_stack(unsigned long *lo, unsigned long *size) { *lo = gexp_host_stack(); *size = GEXP_STACK_SIZE; return 0; }

/* The element segment: every adapter, plus musl's thread entry. */
int __gex_thread_start(int, int, int, long long);
#define GEXP_TABLE(...) \
	__attribute__((used)) gexp_body_t const gexp_table[] = { __VA_ARGS__, gexp__missing, gexp__noop, (gexp_body_t)__gex_thread_start }

#endif
