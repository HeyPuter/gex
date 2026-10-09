/* The C runtime an LP64 wasm zlib needs as an aarch64 program's libz
 * (docs/gexcc.md §7.8): malloc and friends over the provider heap
 * (gex.provider_alloc: fresh guest pages), the mem* functions, and the
 * adapters every replaced library has (missing exports, constructors). */
#include "item3.h"

__attribute__((import_module("gex"), import_name("provider_alloc"))) unsigned gexp_host_alloc(unsigned size);
__attribute__((import_module("gex"), import_name("provider_missing"))) void gexp_host_missing(uint64_t addr);

#define LAST_TARGET 44 /* gex_abi::ctx::LAST_TARGET / 8 */
GEXP(gexp__missing) { gexp_host_missing(A64_X(ctx, LAST_TARGET)); __builtin_trap(); }
/* Bound at the replaced library's constructors and destructors. */
GEXP(gexp__noop) { return a64_ret(ctx, ret, A64_X(ctx, 0)); }

/* Size-class free lists over bump-allocated chunks of provider pages; one
 * lock (the module's data is every thread's). */
static void *freelist[64];
static uint64_t chunks[256][2];
static unsigned nchunks;
static char *cur, *end;
static volatile int lk;
static void lock(void) { while (__atomic_exchange_n(&lk, 1, __ATOMIC_ACQUIRE)) ; }
static void unlock(void) { __atomic_store_n(&lk, 0, __ATOMIC_RELEASE); }
static unsigned cls(size_t n)
{
	if (n <= 16) return 0;
	unsigned b = 64 - __builtin_clzll((unsigned long long)(n - 1)); /* ceil log2 */
	unsigned c = (b - 4) * 2;                                       /* two classes per power */
	if (n - 1 < (size_t)3 << (b - 2)) return c - 1 > 0 ? c - 1 : 1;
	return c;
}
static size_t csize(unsigned c)
{
	if (c == 0) return 16;
	unsigned b = (c + 1) / 2 + 4;
	return (c & 1) ? (size_t)3 << (b - 2) : (size_t)1 << b;
}
void *malloc(size_t n)
{
	unsigned c = cls(n + 16);
	size_t sz = csize(c);
	lock();
	void *p = freelist[c];
	if (p) {
		freelist[c] = *(void **)p;
		unlock();
		return p;
	}
	if ((size_t)(end - cur) < sz) {
		size_t want = (sz + (1 << 20) + 0xffff) & ~(size_t)0xffff;
		char *q = (char *)(uintptr_t)gexp_host_alloc((unsigned)want);
		if (!q) { unlock(); return 0; }
		cur = q; end = q + want;
		if (nchunks < 256) { chunks[nchunks][0] = (uint64_t)(uintptr_t)q; chunks[nchunks][1] = (uint64_t)(uintptr_t)end; nchunks++; }
	}
	size_t *h = (size_t *)cur;
	cur += sz;
	unlock();
	h[0] = c;
	return h + 2;
}
void free(void *p)
{
	if (!p) return;
	unsigned c = (unsigned)((size_t *)p)[-2];
	void **b = (void **)((size_t *)p - 2);
	lock();
	*b = freelist[c];
	freelist[c] = b;
	unlock();
}
void *calloc(size_t a, size_t b)
{
	void *p = malloc(a * b);
	if (p) __builtin_memset(p, 0, a * b);
	return p;
}
void *memcpy(void *d, const void *s, size_t n) { __builtin_memcpy(d, s, n); return d; }
void *memmove(void *d, const void *s, size_t n) { __builtin_memmove(d, s, n); return d; }
void *memset(void *d, int c, size_t n) { __builtin_memset(d, c, n); return d; }
int memcmp(const void *a, const void *b, size_t n)
{
	const unsigned char *x = a, *y = b;
	for (size_t i = 0; i < n; i++) if (x[i] != y[i]) return x[i] - y[i];
	return 0;
}
size_t strlen(const char *s) { size_t n = 0; while (s[n]) n++; return n; }

/* ---- calling guest code ---- */
__attribute__((import_module("gex"), import_name("provider_code_page"))) uint64_t gexp_host_code_page(void);
__attribute__((import_module("gex"), import_name("provider_abi"))) uint64_t gexp_host_abi(unsigned which);
__attribute__((import_module("gex"), import_name("provider_guest_call"))) void gexp_host_guest_call(int delta);

typedef int (*body_t)(int ctx, int32_t depth, uint32_t need, uint64_t ret);

/* The address the host bound to it heads the library's code page (libprov:
 * the page every guest call's return address is in); no thread starts in
 * this library. */
GEXP(__gex_thread_start) { __builtin_trap(); }

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

/* ---- zlib's allocator calls (zalloc.h) ----
 * The stream's zalloc/zfree are either zlib's own (zcalloc, zcfree: this
 * module's functions, which zlib stores when the caller leaves them null)
 * or the caller's: guest code, called as such. A block from this module's
 * heap is never handed to the guest's free, nor a guest block to ours:
 * zcfree frees only what lies in the heap's chunks. */
#include "zutil.h"
static int ours(const void *p)
{
	uint64_t a = (uint64_t)(uintptr_t)p;
	for (unsigned i = 0; i < nchunks; i++)
		if (a >= chunks[i][0] && a < chunks[i][1]) return 1;
	return 0;
}
voidpf gexp_zalloc(z_streamp strm, uInt items, uInt size)
{
	if (strm->zalloc == zcalloc) return zcalloc(strm->opaque, items, size);
	uint64_t a[3] = { (uint64_t)(uintptr_t)strm->opaque, items, size };
	return (voidpf)(uintptr_t)a64_call_guest((uint64_t)(uintptr_t)strm->zalloc, 3, a);
}
void gexp_zfree(z_streamp strm, voidpf p)
{
	if (strm->zfree == zcfree) {
		if (ours(p)) zcfree(strm->opaque, p);
		return;
	}
	uint64_t a[2] = { (uint64_t)(uintptr_t)strm->opaque, (uint64_t)(uintptr_t)p };
	a64_call_guest((uint64_t)(uintptr_t)strm->zfree, 2, a);
}
