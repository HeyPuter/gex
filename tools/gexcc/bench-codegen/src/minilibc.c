/* A small C runtime for the codegen benchmarks (docs/gexcc.md §7.3): the
 * libc functions the kernels call, as a real libc would provide them, so a
 * variant can put them in another module. malloc: size classes with free
 * lists over memory.grow, as fast allocators are. */
#include <stddef.h>
#include <stdint.h>

#define NCLASS 48
static void *freelist[NCLASS];
static char *brk_cur, *brk_end;
size_t heap_bytes;

static unsigned cls(size_t n)
{
	if (n <= 16) return 0;
	unsigned b = 64 - __builtin_clzll((unsigned long long)(n - 1));   /* ceil log2 */
	unsigned c = (b - 4) * 2;                                         /* two classes per power */
	if (n - 1 < (size_t)3 << (b - 2)) return c - 1 > 0 ? c - 1 : 1;
	return c;
}
static size_t csize(unsigned c)
{
	if (c == 0) return 16;
	unsigned b = (c + 1) / 2 + 4;
	return (c & 1) ? (size_t)3 << (b - 2) : (size_t)1 << b;
}
static void *more(size_t n)
{
	if (brk_cur + n > brk_end) {
		size_t want = (n + 0xffff + (1 << 20)) & ~(size_t)0xffff;
#ifdef __wasm__
		size_t old = __builtin_wasm_memory_grow(0, want >> 16);
		if (old == (size_t)-1) return 0;
		char *p = (char *)(old << 16);
#else
		/* a native build (a64main.c): anonymous pages */
		extern void *bench_pages(size_t);
		char *p = bench_pages(want);
		if (!p) return 0;
#endif
		if (p != brk_end) brk_cur = p;
		brk_end = p + want;
	}
	void *r = brk_cur;
	brk_cur += n;
	heap_bytes += n;
	return r;
}
void *malloc(size_t n)
{
	unsigned c = cls(n + 8);
	void *p = freelist[c];
	if (p) { freelist[c] = *(void **)p; return p; }
	size_t *h = more(csize(c));
	if (!h) return 0;
	h[0] = c;
	return (char *)h + 8;
}
void free(void *p)
{
	if (!p) return;
	unsigned c = *(size_t *)((char *)p - 8);
	*(void **)p = freelist[c];
	freelist[c] = p;
}
#ifdef __wasm__
/* memory.copy and memory.fill */
void *memcpy(void *d, const void *s, size_t n) { __builtin_memcpy(d, s, n); return d; }
void *memmove(void *d, const void *s, size_t n) { __builtin_memmove(d, s, n); return d; }
void *memset(void *d, int c, size_t n) { __builtin_memset(d, c, n); return d; }
#else
void *memcpy(void *, const void *, size_t);
void *memset(void *, int, size_t);
/* natively the builtins are calls to these very functions: memcpy and
 * memset are musl's aarch64 assembly (src/a64/), as a Debian arm64 libc's
 * are; memmove is a plain loop (built with -fno-builtin, so it stays one) */
void *memmove(void *d, const void *s, size_t n)
{
	unsigned char *dd = d; const unsigned char *ss = s;
	if (dd == ss || n == 0) return d;
	if (dd + n <= ss || ss + n <= dd) return memcpy(d, s, n);
	if (dd < ss) for (size_t i = 0; i < n; i++) dd[i] = ss[i];
	else for (size_t i = n; i--;) dd[i] = ss[i];
	return d;
}
#endif
void *calloc(size_t a, size_t b) { void *p = malloc(a * b); if (p) memset(p, 0, a * b); return p; }
void *realloc(void *p, size_t n)
{
	if (!p) return malloc(n);
	unsigned c = *(size_t *)((char *)p - 8);
	if (csize(c) - 8 >= n) return p;
	void *q = malloc(n);
	memcpy(q, p, csize(c) - 8);
	free(p);
	return q;
}
int memcmp(const void *a, const void *b, size_t n)
{
	const unsigned char *x = a, *y = b;
	for (; n; n--, x++, y++) if (*x != *y) return *x - *y;
	return 0;
}
size_t strlen(const char *s) { const char *p = s; while (*p) p++; return p - s; }
int strcmp(const char *a, const char *b) { while (*a && *a == *b) a++, b++; return (unsigned char)*a - (unsigned char)*b; }
char *strchr(const char *s, int c) { for (;; s++) { if (*s == (char)c) return (char *)s; if (!*s) return 0; } }
char *strcpy(char *d, const char *s) { char *r = d; while ((*d++ = *s++)); return r; }
char *strdup(const char *s) { size_t n = strlen(s) + 1; char *d = malloc(n); return d ? memcpy(d, s, n) : 0; }
static void swap(char *a, char *b, size_t n) { while (n--) { char t = *a; *a++ = *b; *b++ = t; } }
void qsort(void *base, size_t n, size_t w, int (*cmp)(const void *, const void *))
{
	char *b = base;
	while (n > 1) {
		size_t i = 0, j = n - 1;
		swap(b + (n / 2) * w, b + (n - 1) * w, w);
		char *piv = b + (n - 1) * w;
		for (; i < j;) {
			if (cmp(b + i * w, piv) < 0) i++;
			else { j--; swap(b + i * w, b + j * w, w); }
		}
		swap(b + i * w, piv, w);
		if (i < n - i - 1) { qsort(b, i, w, cmp); b += (i + 1) * w; n -= i + 1; }
		else { qsort(b + (i + 1) * w, n - i - 1, w, cmp); n = i; }
	}
}
/* what zlib/zstd may reference */
int *__errno_location(void) { static int e; return &e; }
void abort(void) { __builtin_trap(); }
void __assert_fail(const char *a, const char *f, int l, const char *fn) { __builtin_trap(); }
