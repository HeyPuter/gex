/* libgt.so: an x86-64 shared library a wasm64 stateful provider (gt.c)
 * replaces under gex (docs/unity-dotnet.md, P2). The native run uses this
 * code; the gex run must print the same with the provider's. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int gt_add(int a, int b) { return a + b; }

long gt_sum8(long a, long b, long c, long d, long e, long f, long g, long h)
{
	return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h;
}

double gt_scale(double x, int k, float y) { return x * k + y; }

const char *gt_greet(const char *who)
{
	static __thread char buf[64];
	snprintf(buf, sizeof buf, "hello, %s (%zu)", who, strlen(who));
	return buf;
}

long gt_counter(void)
{
	static __thread long n;
	return ++n;
}

/* Allocate, fill and checksum on the library's own heap. */
long gt_heap(int n)
{
	long sum = 0;
	for (int round = 0; round < 4; round++) {
		char **v = malloc(n * sizeof *v);
		for (int i = 0; i < n; i++) {
			v[i] = malloc(16 + i % 200);
			memset(v[i], i & 0x7f, 16 + i % 200);
		}
		for (int i = 0; i < n; i++) {
			sum += v[i][i % 16];
			free(v[i]);
		}
		free(v);
	}
	return sum;
}

int gt_write(const char *s) { return (int)write(1, s, strlen(s)); }

int gt_getenv(const char *name, char *out, int n)
{
	const char *v = getenv(name);
	if (!v) return -1;
	snprintf(out, n, "%s", v);
	return (int)strlen(v);
}

static void *worker(void *arg)
{
	long k = (long)arg, s = 0;
	for (long i = 0; i < 1000; i++) s += i * k;
	return (void *)s;
}

/* Threads of the library's own, joined. */
long gt_threads(int n)
{
	pthread_t t[8];
	long total = 0;
	for (int i = 0; i < n; i++) pthread_create(&t[i], 0, worker, (void *)(long)(i + 1));
	for (int i = 0; i < n; i++) { void *r; pthread_join(t[i], &r); total += (long)r; }
	return total;
}

int gt_unprovided(void) { return 42; }

/* Which implementation ran: the test expects "x86-64" natively and the
 * provider's "wasm64" under gex. */
const char *gt_impl(void) { return "x86-64"; }
