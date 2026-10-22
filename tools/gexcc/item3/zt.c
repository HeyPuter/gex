/* item 3's test program: an arm64 Linux program that
 * uses Debian's libz.so.1 -- translated by gex, or replaced by the LP64 wasm
 * build of the same zlib. No libc of its own (raw system calls, its own
 * _start; the dynamic linker still loads libz and the libc it needs).
 * Prints one JSON line: compress2 and uncompress times (median of 5) of
 * 4 MiB of text-like data, and checksums.
 *
 *   zt [VARIANT]
 *   zt checkgz: gzopen/gzwrite/gzread (the LP64 libc's file I/O)
 *   zt check: streams with the program's own zalloc/zfree (zlib calls back
 *   into guest code) and with zlib's, each forked half way through deflate:
 *   parent and child finish it and check the result. One JSON line each. */
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

/* zlib's z_stream, LP64 */
typedef struct {
	const unsigned char *next_in; unsigned avail_in; unsigned long total_in;
	unsigned char *next_out; unsigned avail_out; unsigned long total_out;
	const char *msg; void *state;
	void *(*zalloc)(void *, unsigned, unsigned); void (*zfree)(void *, void *); void *opaque;
	int data_type; unsigned long adler, reserved;
} z_stream;
int deflateInit2_(z_stream *, int, int, int, int, int, const char *, int);
int deflate(z_stream *, int);
int deflateEnd(z_stream *);
int inflateInit_(z_stream *, const char *, int);
int inflate(z_stream *, int);
int inflateEnd(z_stream *);
typedef struct gzFile_s *gzFile;
gzFile gzopen(const char *, const char *);
int gzwrite(gzFile, const void *, unsigned);
int gzread(gzFile, void *, unsigned);
int gzclose(gzFile);

/* The program's allocator: a bump arena; zfree checks it only ever gets
 * the arena's blocks. */
static unsigned char arena[8 << 20];
static unsigned long top, nalloc, nfree, foreign;
static void *my_alloc(void *op, unsigned n, unsigned sz)
{
	(*(unsigned long *)op)++;
	unsigned long b = ((unsigned long)n * sz + 15) & ~15ul;
	if (top + b > sizeof arena) return 0;
	nalloc++;
	void *p = arena + top; top += b; return p;
}
static void my_free(void *op, void *p)
{
	(*(unsigned long *)op)++;
	if ((unsigned char *)p < arena || (unsigned char *)p >= arena + sizeof arena) foreign++;
	nfree++;
}

enum { N = 4 << 20 };
static unsigned char src[N], back[N], dst[N + N / 8 + 1024];

static int check(int custom);
static int checkgz(void);
void cmain(long *sp)
{
	const char *variant = sp[0] > 1 ? (const char *)sp[2] : "zt";
	int is_check = variant[0] == 'c' && variant[1] == 'h' && variant[2] == 'e' && variant[3] == 'c' && variant[4] == 'k';
	static const char *w[] = { "the ", "gex ", "wasm ", "module ", "table ", "memory ", "fork ", "debian ",
		"library ", "function ", "pointer ", "page ", "kernel ", "process ", "thread ", "signal\n" };
	uint32_t x = 12345;
	for (size_t i = 0; i < N;) {
		x = x * 1103515245 + 12345;
		const char *s = w[(x >> 16) & 15];
		while (*s && i < N) src[i++] = *s++;
		if ((x & 0xff) == 7 && i < N) src[i++] = 'A' + (x >> 24) % 26;
	}
	if (is_check) {
		/* check: both; checkc, checkz: the program's allocator, zlib's */
		/* checkgz: zlib's file functions (the library's own open/read/write) */
		int bad = variant[5] == 'g' ? checkgz() :
			(variant[5] != 'z' ? check(1) : 0) | (variant[5] != 'c' ? check(0) : 0);
		sys6(94, bad, 0, 0, 0, 0, 0);
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
static int streq(const unsigned char *a, const unsigned char *b, unsigned long n)
{
	for (unsigned long i = 0; i < n; i++) if (a[i] != b[i]) return 0;
	return 1;
}
/* Deflate src in 64 KiB pieces into dst, forking half way: both processes
 * finish the stream, inflate it and compare. */
static int check(int custom)
{
	unsigned long calls = 0;
	z_stream z = { 0 };
	if (custom) { z.zalloc = my_alloc; z.zfree = my_free; z.opaque = &calls; }
	top = nalloc = nfree = foreign = 0;
	if (deflateInit2_(&z, 6, 8, 15, 8, 0, zlibVersion(), sizeof z)) { put("deflateInit failed\n"); return 1; }
	unsigned long in = 0; long pid = -1;
	z.next_out = dst; z.avail_out = sizeof dst;
	int rc;
	do {
		if (pid < 0 && in >= N / 2) pid = sys6(220, 17, 0, 0, 0, 0, 0); /* fork */
		unsigned k = N - in < 65536 ? N - in : 65536;
		z.next_in = src + in; z.avail_in = k; in += k;
		rc = deflate(&z, in == N ? 4 : 0);
	} while (rc == 0 && in < N);
	unsigned long clen = z.total_out;
	int bad = rc != 1 || deflateEnd(&z) != 0;
	z_stream u = { 0 };
	if (custom) { u.zalloc = my_alloc; u.zfree = my_free; u.opaque = &calls; }
	if (!bad) {
		bad = inflateInit_(&u, zlibVersion(), sizeof u) != 0;
		u.next_in = dst; u.avail_in = clen; u.next_out = back; u.avail_out = N;
		if (!bad) bad = inflate(&u, 4) != 1 || u.total_out != N || inflateEnd(&u) != 0 || !streq(src, back, N);
	}
	if (custom) bad |= nalloc == 0 || nalloc != nfree || foreign || calls != nalloc + nfree;
	if (pid > 0) { int st = 0; sys6(260, pid, (long)&st, 0, 0, 0, 0); bad |= st != 0; }
	len = 0;
	put("{\"check\":\""); put(custom ? "custom" : "zlib"); put("\",\"proc\":\""); put(pid ? "parent" : "child");
	put("\",\"clen\":"); putu(clen); put(",\"allocs\":"); putu(nalloc); put(",\"frees\":"); putu(nfree);
	put(",\"foreign\":"); putu(foreign); put(",\"ok\":"); put(bad ? "false" : "true"); put("}\n");
	sys6(64, 1, (long)buf, len, 0, 0, 0);
	if (pid == 0) sys6(94, bad, 0, 0, 0, 0, 0);
	return bad;
}
static int checkgz(void)
{
	static const char path[] = "/tmp/zt.gz";
	gzFile f = gzopen(path, "wb6");
	int bad = !f;
	for (unsigned long i = 0; !bad && i < N; i += 65536) bad = gzwrite(f, src + i, 65536) != 65536;
	if (f) bad |= gzclose(f) != 0;
	long st[16] = { 0 };
	long fd = sys6(56, -100, (long)path, 0, 0, 0, 0); /* openat */
	if (fd >= 0) { sys6(80, fd, (long)st, 0, 0, 0, 0); sys6(57, fd, 0, 0, 0, 0, 0); } /* fstat, close */
	unsigned long got = 0;
	f = bad ? 0 : gzopen(path, "rb");
	if (f) { int n; while ((n = gzread(f, back + got, 100000)) > 0) got += n; bad |= gzclose(f) != 0; }
	bad |= got != N || !streq(src, back, N);
	len = 0;
	put("{\"check\":\"gz\",\"file\":"); putu(st[6]); put(",\"read\":"); putu(got); put(",\"ok\":"); put(bad ? "false" : "true"); put("}\n");
	sys6(64, 1, (long)buf, len, 0, 0, 0);
	return bad;
}
__asm__(".globl _start\n_start:\n mov x29, #0\n mov x0, sp\n and sp, x0, #-16\n bl cmain\n");
