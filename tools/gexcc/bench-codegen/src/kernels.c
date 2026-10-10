/* Codegen benchmark kernels (docs/gexcc.md §7.3). bench_run(k, n) runs
 * kernel k n times and returns a checksum. */
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include "zlib.h"
#include "zstd.h"

static unsigned char *src, *dst, *back;
static size_t srclen = 4 << 20;

static void init(void)
{
	if (src) return;
	src = malloc(srclen); dst = malloc(srclen * 2); back = malloc(srclen);
	/* text-like data: words from a small vocabulary with a little noise */
	static const char *w[] = { "the ", "gex ", "wasm ", "module ", "table ", "memory ", "fork ", "debian ",
		"library ", "function ", "pointer ", "page ", "kernel ", "process ", "thread ", "signal\n" };
	uint32_t x = 12345;
	size_t i = 0;
	while (i < srclen) {
		x = x * 1103515245 + 12345;
		const char *s = w[(x >> 16) & 15];
		while (*s && i < srclen) src[i++] = *s++;
		if (((x >> 8) & 63) == 0 && i < srclen) src[i++] = (char)(x >> 24);
	}
}

static uint32_t k_zlib(void)
{
	uLongf dl = srclen * 2, bl = srclen;
	compress2(dst, &dl, src, srclen, 6);
	uncompress(back, &bl, dst, dl);
	return (uint32_t)dl ^ (uint32_t)bl ^ back[1234];
}

static uint32_t k_zstd(void)
{
	size_t c = ZSTD_compress(dst, srclen * 2, src, srclen, 3);
	size_t d = ZSTD_decompress(back, srclen, dst, c);
	return (uint32_t)c ^ (uint32_t)d ^ back[4321];
}

/* binary trees: malloc/free heavy, pointer chasing */
struct node { struct node *l, *r; long v; };
static struct node *make(int d)
{
	struct node *n = malloc(sizeof *n);
	n->v = d;
	if (d > 0) { n->l = make(d - 1); n->r = make(d - 1); } else n->l = n->r = 0;
	return n;
}
static long check(struct node *n) { return n->l ? n->v + check(n->l) - check(n->r) : n->v; }
static void del(struct node *n) { if (n->l) { del(n->l); del(n->r); } free(n); }
static uint32_t k_trees(void)
{
	long s = 0;
	for (int i = 0; i < 40; i++) { struct node *t = make(14); s += check(t); del(t); }
	return (uint32_t)s;
}

/* strings: many small libc calls (strlen, strcmp, strdup, memcmp, qsort) */
static int cmps(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }
static uint32_t k_strings(void)
{
	enum { N = 20000 };
	static char *v[N];
	char buf[64];
	uint32_t x = 7, h = 0;
	for (int r = 0; r < 4; r++) {
		for (int i = 0; i < N; i++) {
			x = x * 1103515245 + 12345;
			int len = 4 + (x >> 27);
			for (int j = 0; j < len; j++) buf[j] = 'a' + ((x >> (j % 13)) + j) % 26;
			buf[len] = 0;
			v[i] = strdup(buf);
		}
		qsort(v, N, sizeof v[0], cmps);
		for (int i = 0; i < N; i++) { h = h * 31 + strlen(v[i]) + (strchr(v[i], 'q') != 0); free(v[i]); }
	}
	return h;
}

uint32_t bench_run(int k, int n)
{
	init();
	uint32_t s = 0;
	for (int i = 0; i < n; i++)
		s += k == 0 ? k_zlib() : k == 1 ? k_zstd() : k == 2 ? k_trees() : k_strings();
	return s;
}
