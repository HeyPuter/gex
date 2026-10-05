/* Runtime support every wasm64 provider links (docs/unity-dotnet.md):
 * calls into guest code, and abandoning guest frames on a raise. */
#include "gexprov64.h"

__thread int gexp_ctx;
__thread int32_t gexp_depth;

/* ---- which side of the boundary a thread is on ----
 * gexp_in_guest is 1 while the thread runs guest code. The host stops every
 * thread that is (shims/native libprov: a collection's stop-the-world); a
 * thread crossing into guest code meanwhile parks first. gexp_stop is the
 * host's flag, in this module's memory so the check is a load. */
static __thread volatile int gexp_in_guest;
static volatile int gexp_stop;
__attribute__((import_module("gex"), import_name("provider_thread_register"))) void gexp_host_thread_register(volatile int *in_guest, volatile int *stop);
__attribute__((import_module("gex"), import_name("provider_park"))) void gexp_host_park(void);
__attribute__((import_module("gex"), import_name("provider_stop_guest"))) void gexp_host_stop_guest(void);
__attribute__((import_module("gex"), import_name("provider_start_guest"))) void gexp_host_start_guest(void);
__attribute__((import_module("gex"), import_name("provider_guest_stack_end"))) uint64_t gexp_host_guest_stack_end(uint64_t sp);

/* A runtime's hooks at the crossing (a GC-safe region while in guest code). */
static void no_hook(void) {}
__attribute__((weak, alias("no_hook"))) void gexp_hook_to_guest(void);
__attribute__((weak, alias("no_hook"))) void gexp_hook_from_guest(void);

void gexp_attach_thread(void)
{
	gexp_host_thread_register(&gexp_in_guest, &gexp_stop);
}

void gexp_from_guest(void)
{
	__atomic_store_n(&gexp_in_guest, 0, __ATOMIC_SEQ_CST);
	gexp_hook_from_guest();
}

void gexp_to_guest(void)
{
	gexp_hook_to_guest();
	__atomic_store_n(&gexp_in_guest, 1, __ATOMIC_SEQ_CST);
	if (__atomic_load_n(&gexp_stop, __ATOMIC_SEQ_CST))
		gexp_host_park();
}

void gexp_stop_guest_threads(void)
{
	gexp_host_stop_guest();
}

void gexp_start_guest_threads(void)
{
	gexp_host_start_guest();
}

#include <stdlib.h>
void gexp_thread_guest_info(uint64_t *ctx, uint64_t *stack_end)
{
	/* GEXP_NO_GUEST_ROOTS=1: leave the guest's frames out (a test that
	 * the scanning is what keeps their objects). */
	static int off = -1;
	if (off < 0)
		off = getenv("GEXP_NO_GUEST_ROOTS") != 0;
	if (off) {
		*ctx = *stack_end = 0;
		return;
	}
	*ctx = (uint32_t)gexp_ctx;
	*stack_end = gexp_ctx ? gexp_host_guest_stack_end(GEXP_X(gexp_ctx, 4)) : 0;
}

/* The return address a provider's guest call pushes: in the provider's own
 * code page (its thread entry is at offset 0), one per call depth, so gex's
 * dispatch covers it and its call-out catches a return there that no
 * function body predicted (a longjmp inside the callee, say). */
__attribute__((import_module("gex"), import_name("provider_code_page"))) uint64_t gexp_host_code_page(void);
static uint64_t code_page;

static uint64_t fcache, fcache_bits;

static uint32_t ftab_idx(uint64_t t)
{
	if (__builtin_expect(!fcache, 0)) {
		fcache = gexp_host_abi(GEXP_ABI_FCACHE);
		fcache_bits = gexp_host_abi(GEXP_ABI_FCACHE_BITS);
	}
	uint64_t slot = (uint32_t)((uint32_t)t * 0x9e3779b1u) >> (32 - fcache_bits);
	uint64_t e = __atomic_load_n((uint64_t *)(uintptr_t)(fcache + 8 * slot), __ATOMIC_RELAXED);
	return (e & 0xffffffffull) == (t & 0xffffffffull) ? (uint32_t)(e >> 32) : 0; /* RESOLVE */
}

void gexp_call_guest(uint64_t fn, gexp_call *c)
{
	int ctx = gexp_ctx;
	int32_t depth = gexp_depth;
	static const unsigned char sysv[6] = { 7, 6, 2, 1, 8, 9 }, win[4] = { 1, 2, 8, 9 };
	uint64_t rsp0 = GEXP_X(ctx, 4);
	/* Below the caller's frame (and any red zone), 16-aligned after the
	 * stack arguments, so RSP + 8 is 16-aligned at the callee's entry. */
	uint64_t sp = (rsp0 - 256) & ~15ull;
	unsigned n = c->nstack + (c->win64 ? 4 : 0);
	if (n & 1)
		sp -= 8;
	sp -= 8 * (uint64_t)n;
	for (unsigned i = 0; i < c->nstack; i++)
		GEXP_MEM64(sp + 8 * ((c->win64 ? 4 : 0) + i)) = c->stack[i];
	if (__builtin_expect(!code_page, 0))
		code_page = gexp_host_code_page();
	uint64_t ra = code_page + 0x100 + ((uint64_t)(uint32_t)depth % 0xe00);
	sp -= 8;
	GEXP_MEM64(sp) = ra;
	if (c->win64) {
		for (unsigned i = 0; i < c->nireg && i < 4; i++)
			GEXP_X(ctx, win[i]) = c->ireg[i];
	} else {
		for (unsigned i = 0; i < c->nireg && i < 6; i++)
			GEXP_X(ctx, sysv[i]) = c->ireg[i];
		GEXP_X(ctx, 0) = c->nxmm;
	}
	for (unsigned i = 0; i < c->nxmm && i < 8; i++)
		*(volatile uint64_t *)((uintptr_t)(uint32_t)ctx + GEXP_CTX_V + 16 * i) = c->xmm[i];
	GEXP_X(ctx, 4) = sp;
	gexp_to_guest();
	/* Run the guest until it returns to `ra`. A function body returns 1 when
	 * its return went to the address its caller predicted; otherwise 0, with
	 * PC where the guest continues (a longjmp inside the callee, a signal, a
	 * resume), and the loop goes on from there as gex's own dispatch would. */
	uint64_t pc = fn;
	for (;;) {
		GEXP_X(ctx, GEXP_LAST_TARGET) = pc;
		gexp_body_t f = (gexp_body_t)(uintptr_t)ftab_idx(pc);
		if (f(ctx, depth + 1, 0, ra))
			break;
		pc = GEXP_X(ctx, GEXP_PC);
		if (pc == ra)
			break;
	}
	/* Back from the callee: it popped the return address. Everything the
	 * provider was entered with is the caller's to see again. */
	gexp_ctx = ctx;
	gexp_depth = depth;
	__gex_set_ctx((uint32_t)ctx);
	gexp_from_guest();
	GEXP_X(ctx, 4) = rsp0;
	c->rax = GEXP_X(ctx, 0);
	c->rdx = GEXP_X(ctx, 2);
	c->xmm0 = *(volatile uint64_t *)((uintptr_t)(uint32_t)ctx + GEXP_CTX_V);
	c->xmm1 = *(volatile uint64_t *)((uintptr_t)(uint32_t)ctx + GEXP_CTX_V + 16);
}

/* ---- raising through guest frames ---- */

#define GEXP_RAISE_CODE 0x52414953 /* "RAIS" */

typedef struct catching {
	struct catching *prev;
	uint64_t fn;
	gexp_call *c;
	uint64_t payload;
	int ctx;
	int32_t depth;
	uint64_t saved[16]; /* the guest's general registers */
} catching;

static __thread catching *catch_top;

static void catch_body(void *arg)
{
	catching *k = arg;
	gexp_call_guest(k->fn, k->c);
}

int gexp_catching(void)
{
	return catch_top != 0;
}

uint64_t gexp_call_guest_catching(uint64_t fn, gexp_call *c)
{
	catching k = { catch_top, fn, c, 0, gexp_ctx, gexp_depth };
	for (int i = 0; i < 16; i++)
		k.saved[i] = GEXP_X(k.ctx, i);
	catch_top = &k;
	int code = gexp_host_try(catch_body, &k);
	catch_top = k.prev;
	if (!code)
		return 0;
	/* Unwound: the guest's registers as they were before the call (its
	 * callee-saved ones were mid-function), this thread's state again. */
	for (int i = 0; i < 16; i++)
		GEXP_X(k.ctx, i) = k.saved[i];
	gexp_ctx = k.ctx;
	gexp_depth = k.depth;
	__gex_set_ctx((uint32_t)k.ctx);
	if (code != GEXP_RAISE_CODE)
		gexp_host_throw(code); /* someone else's: keep going */
	return k.payload ? k.payload : 1;
}

_Noreturn void gexp_raise(uint64_t payload)
{
	catch_top->payload = payload;
	gexp_host_throw(GEXP_RAISE_CODE);
}

/* ---- what a function pointer is ---- */

uint64_t gexp_table_base(void);
__attribute__((import_module("gex"), import_name("provider_table_size"))) uint64_t gexp_host_table_size(uint64_t base);

static uint64_t tbase, tsize;

int gexp_is_guest_fn(uint64_t p)
{
	if (__builtin_expect(!tsize, 0)) {
		tbase = gexp_table_base();
		tsize = gexp_host_table_size(tbase);
		if (!tsize)
			__builtin_trap();
	}
	return p - tbase >= tsize;
}

/* ---- reverse slots ---- */

#define GEXP_REVERSE_FIRST 0x1000  /* shims/native libprov REVERSE_FIRST */
#define GEXP_REVERSE_STRIDE 16
#define GEXP_REVERSE_SLOTS ((0x10000 - GEXP_REVERSE_FIRST) / GEXP_REVERSE_STRIDE)

static void *reverse_data[GEXP_REVERSE_SLOTS];
static int reverse_next;

uint64_t gexp_reverse_alloc(void *data)
{
	if (!code_page)
		code_page = gexp_host_code_page();
	int i = __atomic_fetch_add(&reverse_next, 1, __ATOMIC_SEQ_CST);
	if (i >= GEXP_REVERSE_SLOTS)
		return 0;
	reverse_data[i] = data;
	return code_page + GEXP_REVERSE_FIRST + (uint64_t)i * GEXP_REVERSE_STRIDE;
}

void *gexp_reverse_data(uint64_t addr)
{
	if (!code_page)
		code_page = gexp_host_code_page();
	uint64_t i = (addr - code_page - GEXP_REVERSE_FIRST) / GEXP_REVERSE_STRIDE;
	return i < GEXP_REVERSE_SLOTS ? reverse_data[i] : 0;
}

void gexp_reverse_args(int ctx, gexp_call *c)
{
	static const unsigned char sysv[6] = { 7, 6, 2, 1, 8, 9 }, win[4] = { 1, 2, 8, 9 };
	if (c->win64) {
		for (int i = 0; i < 4; i++)
			c->ireg[i] = GEXP_X(ctx, win[i]);
		/* the fifth argument onwards: past the return address and the
		 * 32-byte home area */
		c->stack = (const uint64_t *)(uintptr_t)(GEXP_X(ctx, 4) + 8 + 32);
	} else {
		for (int i = 0; i < 6; i++)
			c->ireg[i] = GEXP_X(ctx, sysv[i]);
		c->stack = (const uint64_t *)(uintptr_t)(GEXP_X(ctx, 4) + 8);
	}
	for (int i = 0; i < 8; i++)
		c->xmm[i] = *(volatile uint64_t *)((uintptr_t)(uint32_t)ctx + GEXP_CTX_V + 16 * i);
}

int gexp_reverse_return(int ctx, uint64_t ret, const gexp_call *c)
{
	GEXP_X(ctx, 0) = c->rax;
	GEXP_X(ctx, 2) = c->rdx;
	*(volatile uint64_t *)((uintptr_t)(uint32_t)ctx + GEXP_CTX_V) = c->xmm0;
	*(volatile uint64_t *)((uintptr_t)(uint32_t)ctx + GEXP_CTX_V + 16) = c->xmm1;
	return gexp_return(ctx, ret);
}

/* ---- signals at a provider's syscalls ----
 * A signal handled at a syscall's return (musl wasm64's syscall stub): gex-kernel has left an x86-64
 * rt_sigframe on the guest stack, RSP at it (its first word the restorer's return address), the handler's
 * arguments in rdi/rsi/rdx and PC at the handler, which is guest code. Run it by the guest ABI, then do
 * what the restorer does (pop the return address, rt_sigreturn), which puts back the interrupted context,
 * the syscall's result and PC included; the loop ends when PC is back where the stub was. Overrides musl's
 * default, which stops. */
#define X86_64_RT_SIGRETURN 15
__attribute__((import_module("gex"), import_name("syscall"))) void gexp_host_syscall_ctx(int ctx, long long pc);

void __gex_signal_frames(volatile unsigned long long *x, unsigned long long pc)
{
	/* A thread of the provider's own (not entered from guest code) has no gexp_ctx yet; this is its context. */
	if (!gexp_ctx)
		gexp_ctx = (int)(uintptr_t)x;
	while (x[32] != pc) {
		uint64_t handler = x[32], frame = x[4];
		gexp_call c = { 0 };
		c.ireg[0] = x[7]; /* signal */
		c.ireg[1] = x[6]; /* &info */
		c.ireg[2] = x[2]; /* &uc */
		c.nireg = 3;
		gexp_call_guest(handler, &c);
		x[4] = frame + 8;
		x[0] = X86_64_RT_SIGRETURN;
		gexp_host_syscall_ctx((int)(uintptr_t)x, 0);
	}
}
