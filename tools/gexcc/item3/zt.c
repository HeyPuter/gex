/* item 3's test program (docs/gexcc.md §7.8): an arm64 Linux program that
 * uses Debian's libz.so.1 -- translated by gex, or replaced by the LP64 wasm
 * build of the same zlib. No libc of its own (raw system calls, its own
 * _start; the dynamic linker still loads libz and the libc it needs).
 * Prints one JSON line: compress2 and uncompress times (median of 5) of
 * 4 MiB of text-like data, and checksums.
 *
 *   zt [VARIANT] */
#include <stddef.h>
#include <stdint.h>
typedef unsigned long uLong;
typedef unsigned long uLongf;
int compress2(unsigned char *dest, uLongf *destLen, const unsigned char *source, uLong sourceLen, int level);
int uncompress(unsigned char *dest, uLongf *destLen, const unsigned char *source, uLong sourceLen);
uLong crc32(uLong crc, const unsigned char *buf, unsigned len);
uLong compressBound(uLong sourceLen);
const char *zlibVersion(void);

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
static char buf[512];
static int len;
static void put(const char *s) { while (*s) buf[len++] = *s++; }
static void putu(unsigned long v) { char t[24]; int n = 0; do t[n++] = '0' + v % 10; while (v /= 10); while (n) buf[len++] = t[--n]; }
static void putms(double v) { unsigned long c = (unsigned long)(v * 100 + 0.5); putu(c / 100); buf[len++] = '.'; putu(c / 10 % 10); putu(c % 10); }
static void sort5(double *t) { for (int i = 0; i < 5; i++) for (int j = i + 1; j < 5; j++) if (t[j] < t[i]) { double x = t[i]; t[i] = t[j]; t[j] = x; } }

enum { N = 4 << 20 };
static unsigned char src[N], back[N], dst[N + N / 8 + 1024];

void cmain(long *sp)
{
	const char *variant = sp[0] > 1 ? (const char *)sp[2] : "zt";
	static const char *w[] = { "the ", "gex ", "wasm ", "module ", "table ", "memory ", "fork ", "debian ",
		"library ", "function ", "pointer ", "page ", "kernel ", "process ", "thread ", "signal\n" };
	uint32_t x = 12345;
	for (size_t i = 0; i < N;) {
		x = x * 1103515245 + 12345;
		const char *s = w[(x >> 16) & 15];
		while (*s && i < N) src[i++] = *s++;
		if ((x & 0xff) == 7 && i < N) src[i++] = 'A' + (x >> 24) % 26;
	}
	double tc[5], tu[5];
	uLongf clen = 0, ulen = 0;
	for (int r = 0; r < 6; r++) {
		double t0 = now_ms();
		clen = sizeof dst;
		if (compress2(dst, &clen, src, N, 6)) { put("compress2 failed\n"); break; }
		double t1 = now_ms();
		ulen = N;
		if (uncompress(back, &ulen, dst, clen)) { put("uncompress failed\n"); break; }
		double t2 = now_ms();
		if (r) { tc[r - 1] = t1 - t0; tu[r - 1] = t2 - t1; }
	}
	sort5(tc); sort5(tu);
	put("{\"variant\":\""); put(variant); put("\",\"zlib\":\""); put(zlibVersion()); put("\"");
	put(",\"compress\":"); putms(tc[2]); put(",\"uncompress\":"); putms(tu[2]);
	put(",\"clen\":"); putu(clen); put(",\"crc\":"); putu(crc32(0, back, ulen)); put("}\n");
	sys6(64, 1, (long)buf, len, 0, 0, 0);
	sys6(94, 0, 0, 0, 0, 0, 0);
	for (;;) ;
}
__asm__(".globl _start\n_start:\n mov x29, #0\n mov x0, sp\n and sp, x0, #-16\n bl cmain\n");
