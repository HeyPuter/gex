/* Threads that do not fight over anything.
 *
 * The smallest program that can tell whether a thread ran at all in a Worker
 * of its own: each one writes only its own slot, and the main thread joins
 * them and adds up. No mutex, no condition variable, one futex per join.
 *
 * `threads.c` next door is the hard version and shares a mutex twenty
 * thousand times per thread. When both fail, this one says whether the
 * mechanism is broken or only the contention is, and those are days apart.
 */
#include <pthread.h>
#include <stdio.h>

#define N 4
#define ITERS 100000

struct arg { int id; long sum; int tid_seen; };
static __thread int mine;

static void *worker(void *p) {
    struct arg *a = p;
    /* Thread-local storage, which is the one piece of per-thread state that
       is not registers: each thread must see only its own. */
    mine = a->id;
    for (int i = 0; i < ITERS; i++) a->sum += i;
    a->tid_seen = mine;
    return NULL;
}

int main(void) {
    pthread_t t[N];
    struct arg args[N];
    for (int i = 0; i < N; i++) {
        args[i] = (struct arg){.id = i + 1, .sum = 0, .tid_seen = -1};
        if (pthread_create(&t[i], NULL, worker, &args[i]) != 0) {
            printf("pthread_create failed\n");
            return 1;
        }
    }
    long total = 0;
    int tls_ok = 1;
    for (int i = 0; i < N; i++) {
        pthread_join(t[i], NULL);
        total += args[i].sum;
        if (args[i].tid_seen != i + 1) tls_ok = 0;
    }
    printf("sum %ld expected %ld\n", total, (long)N * ITERS * (ITERS - 1) / 2);
    printf("tls %s\n", tls_ok ? "ok" : "WRONG");
    printf("main tls %d\n", mine);
    printf("done\n");
    return 0;
}
