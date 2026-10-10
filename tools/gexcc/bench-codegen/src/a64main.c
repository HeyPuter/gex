/* The codegen kernels as a native Linux program (aarch64 or x86-64), for
 * running translated by gex next to the wasm variants (docs/gexcc.md 7.3):
 * freestanding, raw system calls, prints the harness's JSON line. */
#include <stddef.h>
#include <stdint.h>
uint32_t bench_run(int k, int n);

#if defined(__aarch64__)
static long sys6(long n, long a, long b, long c, long d, long e, long f)
{
	register long x8 __asm__("x8") = n, x0 __asm__("x0") = a, x1 __asm__("x1") = b, x2 __asm__("x2") = c,
		x3 __asm__("x3") = d, x4 __asm__("x4") = e, x5 __asm__("x5") = f;
	__asm__ volatile("svc 0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4), "r"(x5) : "memory");
	return x0;
}
#define SYS_write 64
#define SYS_exit 93
#define SYS_mmap 222
#define SYS_clock_gettime 113
__asm__(".globl _start\n_start:\n mov x29, #0\n mov x0, sp\n and sp, x0, #-16\n bl cmain\n");
#endif

void *bench_pages(size_t n)
{
	long r = sys6(SYS_mmap, 0, n, 3, 0x22, -1, 0);
	return r < 0 && r > -4096 ? 0 : (void *)r;
}
static double now_ms(void)
{
	struct { long s, ns; } t;
	sys6(SYS_clock_gettime, 1, (long)&t, 0, 0, 0, 0);
	return t.s * 1e3 + t.ns / 1e6;
}
static char buf[512];
static int len;
static void put(const char *s) { while (*s) buf[len++] = *s++; }
static void putu(unsigned long v) { char t[24]; int n = 0; do t[n++] = '0' + v % 10; while (v /= 10); while (n) buf[len++] = t[--n]; }
static void putms(double v) { unsigned long c = (unsigned long)(v * 100 + 0.5); putu(c / 100); buf[len++] = '.'; putu(c / 10 % 10); putu(c % 10); }

void cmain(long *sp)
{
	const char *names[] = { "zlib", "zstd", "trees", "strings" };
	const char *variant = sp[0] > 1 ? (const char *)sp[2] : "native";
	put("{\"variant\":\""); put(variant); put("\"");
	/* argv[2]: one kernel only (0..3) */
	int only = sp[0] > 2 ? *(const char *)sp[3] - '0' : -1;
	for (int k = 0; k < 4; k++) {
		if (only >= 0 && k != only) continue;
		bench_run(k, 1);
		double t[5]; uint32_t sum = 0;
		for (int r = 0; r < 5; r++) { double t0 = now_ms(); sum = bench_run(k, 1); t[r] = now_ms() - t0; }
		for (int i = 0; i < 5; i++) for (int j = i + 1; j < 5; j++) if (t[j] < t[i]) { double x = t[i]; t[i] = t[j]; t[j] = x; }
		put(",\""); put(names[k]); put("\":"); putms(t[2]);
		put(",\""); put(names[k]); put("_sum\":"); putu(sum);
	}
	put("}\n");
	sys6(SYS_write, 1, (long)buf, len, 0, 0, 0);
	sys6(SYS_exit, 0, 0, 0, 0, 0, 0);
	for (;;) ;
}
