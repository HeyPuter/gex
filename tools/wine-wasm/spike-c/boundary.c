/* W0(c): the cost of the native/guest boundary and of the ICALL check,
 * measured in V8 (docs/wine-native.md §3.3, §4).
 *
 * One module stands in for three kinds of code, all with gex's real calling
 * sequences (docs/merge-plan.md §9.8.1):
 *   - native Wine code: plain C, C ABI, function pointers are table indices;
 *   - an AOT unit (application x86 code): F_body(ctx, depth, need, ret)
 *     functions that keep i386 registers in ctx and arguments on the guest
 *     stack in linear memory, and reach other functions through the fcache
 *     probe + ctx.LAST_TARGET + call_indirect;
 *   - the generated glue: an entry adapter (x86 -> native, from a .spec
 *     signature) and an exit thunk (native -> x86, from a wasm signature).
 *
 * Every benchmark loop calls a trivial callee N times; bench.mjs reports
 * ns per call. Built by build.sh with the same flags as gex-cgen's units. */
#include <stdint.h>

#define CTX 0x1000u
#define X(n) (*(volatile uint64_t *)(uintptr_t)(CTX + 8 * (n)))
#define EAX X(0)
#define ESP X(4)
#define LAST_TARGET X(44)
#define FCACHE 0x100000u
#define FCACHE_MUL 0x9e3779b1u
#define G32(a) (*(volatile uint32_t *)(uintptr_t)(a))

/* Below this a function pointer is a table index (native code); at or
 * above it, a guest address. gex's guest images start at 0x400000. */
#define ICALL_LIMIT 0x400000u

typedef int (*fbody)(int ctx, int depth, int need, int64_t ret);
typedef int (*ii_fn)(int, int);

static inline uint32_t fcache_slot(uint32_t t) { return (t * FCACHE_MUL) >> 17; }

/* The probe a unit does before every call that is not module-local. */
static inline int probe(uint32_t t)
{
	uint64_t e = *(volatile uint64_t *)(uintptr_t)(FCACHE + 8 * fcache_slot(t));
	return (uint32_t)e == t ? (int)(e >> 32) : 0;
}

static inline fbody tab(int idx) { return (fbody)(uintptr_t)idx; }

/* ---- native callees ---------------------------------------------------- */

__attribute__((noinline)) int native_add(int a, int b) { return a * 3 + b; }

/* ---- guest callee: what an AOT unit's body for `cdecl int f(int a,int b)`
 * looks like at its edges -------------------------------------------- */

__attribute__((noinline)) int guest_add(int ctx, int depth, int need, int64_t ret)
{
	uint32_t sp = (uint32_t)ESP;
	uint32_t ra = G32(sp);
	int a = G32(sp + 4), b = G32(sp + 8);
	EAX = (uint32_t)(a * 3 + b);
	ESP = sp + 4;                 /* ret */
	return ra == (uint64_t)ret;   /* the return check */
}

/* ---- entry adapter: x86 -> native, generated from
 *      @ cdecl native_add(long long) -------------------------------- */

__attribute__((noinline)) int adapter_native_add(int ctx, int depth, int need, int64_t ret)
{
	uint32_t sp = (uint32_t)ESP;
	uint32_t ra = G32(sp);
	EAX = (uint32_t)native_add(G32(sp + 4), G32(sp + 8));
	ESP = sp + 4;
	return ra == (uint64_t)ret;
}

/* ---- exit thunk: native -> x86, generated per wasm signature (i32,i32)->i32
 * ------------------------------------------------------------------------ */

#define RET_TOKEN 0x003f0010u   /* a return address no guest code has */

__attribute__((noinline, cold)) int exit_ii(uint32_t target, int a, int b)
{
	uint32_t sp = (uint32_t)ESP;
	sp -= 12;
	G32(sp) = RET_TOKEN;
	G32(sp + 4) = a;
	G32(sp + 8) = b;
	ESP = sp;
	LAST_TARGET = target;
	int ok = tab(probe(target))(CTX, 1, 0, RET_TOKEN);
	if (!ok) __builtin_trap();     /* the guest left another way: unwind */
	ESP = (uint32_t)ESP + 8;       /* cdecl: the caller pops */
	return (int)(uint32_t)EAX;
}

/* The ICALL check at a native call site. */
#define ICALL_II(fp, a, b) \
	(__builtin_expect((uintptr_t)(fp) < ICALL_LIMIT, 1) ? (fp)(a, b) : exit_ii((uint32_t)(uintptr_t)(fp), a, b))

/* The branch-free form: one call_indirect whose index is the target when
 * it is native, or the signature's exit stub when it is a guest address;
 * the stub takes the address from a global (per instance, so per thread).
 * No second call at the site, so V8 keeps the loop's values in registers
 * and can still inline a monomorphic native target. */
static uint32_t __attribute__((address_space(1))) icall_target;  /* a wasm global */
__attribute__((noinline)) static int exit_stub_ii(int a, int b) { return exit_ii(icall_target, a, b); }
static inline ii_fn icall_select_ii(ii_fn fp)
{
	uintptr_t p = (uintptr_t)fp;
	icall_target = p;
	return p < ICALL_LIMIT ? fp : exit_stub_ii;
}
#define ICALL2_II(fp, a, b) (icall_select_ii(fp)(a, b))

/* ---- the benchmarks ---------------------------------------------------- */

/* The function pointers come from memory, so clang cannot see through them. */
volatile ii_fn g_native_fp;
volatile uint32_t g_guest_addr, g_adapter_addr;

int bench_native_direct(int n)
{
	int s = 0;
	_Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) s = native_add(i, s);
	return s;
}

int bench_native_indirect(int n)
{
	ii_fn f = g_native_fp;
	int s = 0;
	_Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) { s = f(i, s); __asm__ volatile("" ::: "memory"); f = g_native_fp; }
	return s;
}

int bench_native_icall(int n)
{
	ii_fn f = g_native_fp;
	int s = 0;
	_Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) { s = ICALL_II(f, i, s); __asm__ volatile("" ::: "memory"); f = g_native_fp; }
	return s;
}

int bench_native_icall2(int n)
{
	ii_fn f = g_native_fp;
	int s = 0;
	_Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) { s = ICALL2_II(f, i, s); __asm__ volatile("" ::: "memory"); f = g_native_fp; }
	return s;
}

int bench_native_to_guest2(int n)
{
	ii_fn f = (ii_fn)(uintptr_t)g_guest_addr;
	int s = 0;
	_Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) { s = ICALL2_II(f, i, s); __asm__ volatile("" ::: "memory"); f = (ii_fn)(uintptr_t)g_guest_addr; }
	return s;
}

int bench_native_to_guest(int n)
{
	ii_fn f = (ii_fn)(uintptr_t)g_guest_addr;
	int s = 0;
	_Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) { s = ICALL_II(f, i, s); __asm__ volatile("" ::: "memory"); f = (ii_fn)(uintptr_t)g_guest_addr; }
	return s;
}

/* A unit's call site: push the arguments and the return address, probe,
 * call, check, reload ESP, pop the arguments (cdecl). */
static inline int unit_call(uint32_t target, int a, int b, uint32_t ra)
{
	uint32_t sp = (uint32_t)ESP - 12;
	G32(sp) = ra;
	G32(sp + 4) = a;
	G32(sp + 8) = b;
	ESP = sp;
	LAST_TARGET = target;
	if (!tab(probe(target))(CTX, 1, 0, ra)) __builtin_trap();
	ESP = (uint32_t)ESP + 8;
	return (int)(uint32_t)EAX;
}

int bench_guest_to_guest(int n)
{
	int s = 0;
	_Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) s = unit_call(g_guest_addr, i, s, 0x401234);
	return s;
}

int bench_guest_to_native(int n)
{
	int s = 0;
	_Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) s = unit_call(g_adapter_addr, i, s, 0x401234);
	return s;
}

/* ---- setup: what the runtime does at install --------------------------- */

static void fcache_put(uint32_t addr, int idx)
{
	*(volatile uint64_t *)(uintptr_t)(FCACHE + 8 * fcache_slot(addr)) = ((uint64_t)idx << 32) | addr;
}

void setup(void)
{
	ESP = 0x300000;
	g_native_fp = native_add;
	g_guest_addr = 0x00401000;     /* the guest function's address */
	g_adapter_addr = 0x7b801000;   /* a Wine export's address in its PE skeleton */
	fcache_put(g_guest_addr, (int)(uintptr_t)guest_add);
	fcache_put(g_adapter_addr, (int)(uintptr_t)adapter_native_add);
}

#ifdef GEXW_ICALL_PASS
/* The same loop as bench_native_indirect, written plainly: the ICALL pass
 * (tools/wine-wasm/icall) rewrites the call. Its exit stubs call these. */
uint64_t __gexw_guest_call(uint32_t target, const uint32_t *w, uint32_t n)
{
    return (uint32_t)exit_ii(target, (int)w[0], (int)w[1]);
}
double __gexw_guest_call_fp(uint32_t target, const uint32_t *w, uint32_t n) { return 0; }

int bench_native_pass(int n)
{
    ii_fn f = g_native_fp;
    int s = 0;
    _Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) { s = f(i, s); __asm__ volatile("" ::: "memory"); f = g_native_fp; }
    return s;
}

int bench_native_to_guest_pass(int n)
{
    ii_fn f = (ii_fn)(uintptr_t)g_guest_addr;
    int s = 0;
    _Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++) { s = f(i, s); __asm__ volatile("" ::: "memory"); f = (ii_fn)(uintptr_t)g_guest_addr; }
    return s;
}
#endif
