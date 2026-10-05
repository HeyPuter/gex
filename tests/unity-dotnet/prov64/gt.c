/* gt.wasm: libgt.so's exports as a wasm64 stateful provider (docs/unity-
 * dotnet.md, P2): the same behaviour as libgt.c, in wasm, on the provider's
 * own libc (musl wasm64), heap, TLS and threads. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include "gexprov64.h"

static const char *greet(const char *who)
{
	static __thread char buf[64];
	snprintf(buf, sizeof buf, "hello, %s (%zu)", who, strlen(who));
	return buf;
}

static long counter(void)
{
	static __thread long n;
	return ++n;
}

static long heap(int n)
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

static void *worker(void *arg)
{
	long k = (long)arg, s = 0;
	for (long i = 0; i < 1000; i++) s += i * k;
	return (void *)s;
}

static long threads(int n)
{
	pthread_t t[8];
	long total = 0;
	for (int i = 0; i < n; i++) pthread_create(&t[i], 0, worker, (void *)(long)(i + 1));
	for (int i = 0; i < n; i++) { void *r; pthread_join(t[i], &r); total += (long)r; }
	return total;
}

#define A(i) GEXP_ARG(ctx, i)

GEXP_SYSV(gexp_gt_add) { gexp_enter(ctx); GEXP_RET_U64(ctx, (uint32_t)((int)A(0) + (int)A(1))); return gexp_return(ctx, ret); }
GEXP_SYSV(gexp_gt_sum8)
{
	gexp_enter(ctx);
	long r = 0;
	for (unsigned i = 0; i < 8; i++) r += (long)(i + 1) * (long)A(i);
	GEXP_RET_U64(ctx, r);
	return gexp_return(ctx, ret);
}
GEXP_SYSV(gexp_gt_scale)
{
	gexp_enter(ctx);
	GEXP_RET_F64(ctx, GEXP_XMM_D(ctx, 0) * (int)A(0) + GEXP_XMM_F(ctx, 1));
	return gexp_return(ctx, ret);
}
GEXP_SYSV(gexp_gt_greet) { gexp_enter(ctx); GEXP_RET_U64(ctx, (uintptr_t)greet((const char *)A(0))); return gexp_return(ctx, ret); }
GEXP_SYSV(gexp_gt_counter) { gexp_enter(ctx); GEXP_RET_U64(ctx, counter()); return gexp_return(ctx, ret); }
GEXP_SYSV(gexp_gt_heap) { gexp_enter(ctx); GEXP_RET_U64(ctx, heap((int)A(0))); return gexp_return(ctx, ret); }
GEXP_SYSV(gexp_gt_write)
{
	gexp_enter(ctx);
	const char *s = (const char *)A(0);
	GEXP_RET_U64(ctx, (uint32_t)(int)write(1, s, strlen(s)));
	return gexp_return(ctx, ret);
}
GEXP_SYSV(gexp_gt_getenv)
{
	gexp_enter(ctx);
	const char *v = getenv((const char *)A(0));
	int r = -1;
	if (v) {
		snprintf((char *)A(1), (int)A(2), "%s", v);
		r = (int)strlen(v);
	}
	GEXP_RET_U64(ctx, (uint32_t)r);
	return gexp_return(ctx, ret);
}
GEXP_SYSV(gexp_gt_threads) { gexp_enter(ctx); GEXP_RET_U64(ctx, threads((int)A(0))); return gexp_return(ctx, ret); }

GEXP_SYSV(gexp_gt_impl) { gexp_enter(ctx); GEXP_RET_U64(ctx, (uintptr_t)"wasm64"); return gexp_return(ctx, ret); }

GEXP_DEFINE_MISSING
GEXP_DEFINE_THREAD_ENTRY
GEXP_TABLE(gexp_gt_add, gexp_gt_sum8, gexp_gt_scale, gexp_gt_greet, gexp_gt_counter, gexp_gt_heap, gexp_gt_write,
	gexp_gt_getenv, gexp_gt_threads, gexp_gt_impl);
