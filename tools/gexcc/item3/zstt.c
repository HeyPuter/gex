/* item 3's zstd test program: an arm64 Linux program on Debian's
 * libzstd.so.1 -- translated by gex, or replaced by the LP64 wasm build of
 * the same zstd (build-zstd.sh). No libc of its own, as zt.c.
 *
 *   zstt [VARIANT]  one JSON line: ZSTD_compress (level 3) and
 *                   ZSTD_decompress of 4 MiB of text-like data, ms (median
 *                   of 5 after a warm-up), and checksums
 *   zstt check      streaming contexts with the program's own allocator
 *                   (ZSTD_customMem: zstd calls back into guest code) and
 *                   with zstd's, each forked half way through compression,
 *                   finished and checked in both processes; and the struct
 *                   by value calls (ZSTD_getCParams, ZSTD_cParam_getBounds,
 *                   ZSTD_createCCtx_advanced). One JSON line each. */
#define ZSTD_STATIC_LINKING_ONLY
#include "zstd.h"
#include <stdint.h>

static long sys6(long n, long a, long b, long c, long d, long e, long f)
{
	register long x8 __asm__("x8") = n, x0 __asm__("x0") = a, x1 __asm__("x1") = b, x2 __asm__("x2") = c,
		x3 __asm__("x3") = d, x4 __asm__("x4") = e, x5 __asm__("x5") = f;
	__asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5) : "memory");
	return x0;
}
static double now_ms(void)
{
	struct { long s, ns; } t;
	sys6(113, 1, (long)&t, 0, 0, 0, 0);
	return t.s * 1e3 + t.ns / 1e6;
}
static char buf[1024];
static int len;
static void put(const char *s) { while (*s) buf[len++] = *s++; }
static void putu(unsigned long v) { char t[24]; int n = 0; do t[n++] = '0' + v % 10; while (v /= 10); while (n) buf[len++] = t[--n]; }
static void puti(long v) { if (v < 0) { buf[len++] = '-'; v = -v; } putu(v); }
static void putms(double v) { unsigned long c = (unsigned long)(v * 100 + 0.5); putu(c / 100); buf[len++] = '.'; putu(c / 10 % 10); putu(c % 10); }
static void flush(void) { sys6(64, 1, (long)buf, len, 0, 0, 0); len = 0; }
static void sort5(double *t) { for (int i = 0; i < 5; i++) for (int j = i + 1; j < 5; j++) if (t[j] < t[i]) { double x = t[i]; t[i] = t[j]; t[j] = x; } }
static unsigned long fnv(const unsigned char *p, unsigned long n) { unsigned long h = 1469598103934665603ul; while (n--) h = (h ^ *p++) * 1099511628211ul; return h; }

/* The program's allocator: a bump arena; free checks it only ever gets the
 * arena's blocks. */
static unsigned char arena[24 << 20] __attribute__((aligned(16)));
static unsigned long top, nalloc, nfree, foreign, calls;
static void *my_alloc(void *op, size_t sz)
{
	(*(unsigned long *)op)++;
	unsigned long b = (sz + 15) & ~15ul;
	if (top + b > sizeof arena) return 0;
	nalloc++;
	void *p = arena + top; top += b; return p;
}
static void my_free(void *op, void *p)
{
	(*(unsigned long *)op)++;
	if (!p) return;
	if ((unsigned char *)p < arena || (unsigned char *)p >= arena + sizeof arena) foreign++;
	nfree++;
}

enum { N = 4 << 20 };
static unsigned char src[N], back[N], dst[N + N / 8 + 65536];

static int eq(const unsigned char *a, const unsigned char *b, unsigned long n) { while (n--) if (*a++ != *b++) return 0; return 1; }

static int check(int custom)
{
	ZSTD_customMem cm = { my_alloc, my_free, &calls };
	top = nalloc = nfree = foreign = calls = 0;
	ZSTD_CCtx *c = custom ? ZSTD_createCCtx_advanced(cm) : ZSTD_createCCtx();
	int bad = !c;
	if (!bad) bad = ZSTD_isError(ZSTD_CCtx_setParameter(c, ZSTD_c_compressionLevel, 5)) ||
		ZSTD_isError(ZSTD_CCtx_setParameter(c, ZSTD_c_checksumFlag, 1));
	ZSTD_outBuffer out = { dst, sizeof dst, 0 };
	unsigned long in = 0; long pid = -1;
	while (!bad && in < N) {
		if (pid < 0 && in >= N / 2) pid = sys6(220, 17, 0, 0, 0, 0, 0); /* fork */
		unsigned long k = N - in < 65536 ? N - in : 65536;
		ZSTD_inBuffer ib = { src + in, k, 0 };
		in += k;
		ZSTD_EndDirective e = in == N ? ZSTD_e_end : ZSTD_e_continue;
		size_t r;
		do r = ZSTD_compressStream2(c, &out, &ib, e); while (!ZSTD_isError(r) && (e == ZSTD_e_end ? r != 0 : ib.pos < ib.size));
		bad = ZSTD_isError(r);
	}
	unsigned long clen = out.pos;
	if (c) ZSTD_freeCCtx(c);
	if (!bad) {
		ZSTD_DCtx *d = custom ? ZSTD_createDCtx_advanced(cm) : ZSTD_createDCtx();
		ZSTD_inBuffer ib = { dst, clen, 0 };
		ZSTD_outBuffer ob = { back, N, 0 };
		size_t r = ZSTD_decompressStream(d, &ob, &ib);
		bad = !d || r != 0 || ob.pos != N || !eq(src, back, N);
		ZSTD_freeDCtx(d);
	}
	if (custom) bad |= nalloc == 0 || nalloc != nfree || foreign || calls < nalloc + nfree;
	if (pid > 0) { int st = 0; sys6(260, pid, (long)&st, 0, 0, 0, 0); bad |= st != 0; }
	put("{\"check\":\""); put(custom ? "custom" : "zstd"); put("\",\"proc\":\""); put(pid ? "parent" : "child");
	put("\",\"clen\":"); putu(clen); put(",\"hash\":"); putu(fnv(dst, clen) & 0xffffffff);
	put(",\"allocs\":"); putu(nalloc); put(",\"frees\":"); putu(nfree);
	put(",\"foreign\":"); putu(foreign); put(",\"ok\":"); put(bad ? "false" : "true"); put("}\n");
	flush();
	if (pid == 0) sys6(94, bad, 0, 0, 0, 0, 0);
	return bad;
}

/* Structs by value: a 28-byte result (through x8), a 16-byte one (x0, x1). */
static int structs(void)
{
	ZSTD_compressionParameters p = ZSTD_getCParams(19, N, 0);
	ZSTD_bounds b = ZSTD_cParam_getBounds(ZSTD_c_compressionLevel);
	ZSTD_parameters q = ZSTD_getParams(3, 0, 0);
	size_t est = ZSTD_estimateCCtxSize_usingCParams(p);
	put("{\"cparams19\":["); putu(p.windowLog); put(","); putu(p.chainLog); put(","); putu(p.hashLog); put(",");
	putu(p.searchLog); put(","); putu(p.minMatch); put(","); putu(p.targetLength); put(","); putu(p.strategy);
	put("],\"levels\":["); puti(b.lowerBound); put(","); puti(b.upperBound); put("],\"err\":"); putu(ZSTD_isError(b.error));
	put(",\"params3\":["); putu(q.cParams.windowLog); put(","); putu(q.fParams.contentSizeFlag); put("],\"cctx19\":"); putu(est);
	put(",\"version\":\""); put(ZSTD_versionString()); put("\"}\n");
	flush();
	return ZSTD_isError(b.error) || b.upperBound < 19 || p.windowLog < 20;
}

void cmain(long *sp)
{
	const char *variant = sp[0] > 1 ? (const char *)sp[2] : "zstt";
	static const char *w[] = { "the ", "gex ", "wasm ", "module ", "table ", "memory ", "fork ", "debian ",
		"library ", "function ", "pointer ", "page ", "kernel ", "process ", "thread ", "signal\n" };
	uint32_t x = 12345;
	for (unsigned long i = 0; i < N;) {
		x = x * 1103515245 + 12345;
		const char *s = w[(x >> 16) & 15];
		while (*s && i < N) src[i++] = *s++;
		if ((x & 0xff) == 7 && i < N) src[i++] = 'A' + (x >> 24) % 26;
	}
	if (variant[0] == 'c' && variant[1] == 'h' && variant[2] == 'e' && variant[3] == 'c' && variant[4] == 'k') {
		int bad = structs() | check(1) | check(0);
		sys6(94, bad, 0, 0, 0, 0, 0);
	}
	double tc[5], tu[5];
	size_t clen = 0, ulen = 0;
	for (int r = 0; r < 6; r++) {
		double t0 = now_ms();
		clen = ZSTD_compress(dst, sizeof dst, src, N, 3);
		double t1 = now_ms();
		ulen = ZSTD_decompress(back, N, dst, clen);
		double t2 = now_ms();
		if (ZSTD_isError(clen) || ulen != N) { put("zstd failed\n"); flush(); sys6(94, 1, 0, 0, 0, 0, 0); }
		if (r) { tc[r - 1] = t1 - t0; tu[r - 1] = t2 - t1; }
	}
	sort5(tc); sort5(tu);
	put("{\"variant\":\""); put(variant); put("\",\"zstd\":\""); put(ZSTD_versionString()); put("\"");
	put(",\"compress\":"); putms(tc[2]); put(",\"decompress\":"); putms(tu[2]);
	put(",\"clen\":"); putu(clen); put(",\"hash\":"); putu(fnv(back, ulen) & 0xffffffff); put("}\n");
	flush();
	sys6(94, 0, 0, 0, 0, 0, 0);
	for (;;) ;
}
__asm__(".globl _start\n_start:\n mov x29, #0\n mov x0, sp\n and sp, x0, #-16\n bl cmain\n");
