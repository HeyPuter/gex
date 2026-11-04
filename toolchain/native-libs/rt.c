/* The C runtime an LP64 wasm library needs in place of an aarch64
 * program's (zlib, zstd): malloc and friends over the provider heap
 * (gex.provider_alloc: fresh guest pages), the mem* functions, and the
 * adapters every replaced library has (missing exports, constructors). */
#include "native-libs.h"

__attribute__((import_module("gex"), import_name("provider_alloc"))) unsigned gexp_host_alloc(unsigned size);
__attribute__((import_module("gex"), import_name("provider_missing"))) void gexp_host_missing(uint64_t addr);

GEXP(gexp__missing) { gexp_host_missing(A64_X(ctx, LAST_TARGET)); __builtin_trap(); }
/* Bound at the replaced library's constructors and destructors. */
GEXP(gexp__noop) { return a64_ret(ctx, ret, A64_X(ctx, 0)); }

/* Size-class free lists over bump-allocated chunks of provider pages; one
 * lock (the module's data is every thread's). */
static void *freelist[64];
static uint64_t chunks[4096][2];
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
		if (nchunks < 4096) { chunks[nchunks][0] = (uint64_t)(uintptr_t)q; chunks[nchunks][1] = (uint64_t)(uintptr_t)end; nchunks++; }
	}
	size_t *h = (size_t *)cur;
	cur += sz;
	unlock();
	h[0] = c;
	return h + 2;
}
/* Only this heap's blocks: anything else (a guest pointer that reached
 * here) is left alone. */
void free(void *p)
{
	if (!p || !gexp_heap_owns(p)) return;
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

/* Whether `p` is a block of this module's heap (a pointer zlib or zstd
 * hands to its own free that the guest's allocator made is never freed
 * here). */
int gexp_heap_owns(const void *p)
{
	uint64_t a = (uint64_t)(uintptr_t)p;
	for (unsigned i = 0; i < nchunks; i++)
		if (a >= chunks[i][0] && a < chunks[i][1]) return 1;
	return 0;
}

/* The address the host bound to it heads the library's code page (libprov:
 * the page every guest call's return address is in); no thread starts in
 * this library. */
GEXP(__gex_thread_start) { __builtin_trap(); }
