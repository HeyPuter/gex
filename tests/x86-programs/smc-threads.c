/* Self-modifying code on several threads sharing one page, as a JIT's code
 * area is shared: each thread rewrites its own small function there (`mov
 * eax, N; ret`, a new N each time) and calls it. Every rewrite makes a block
 * guard see new bytes and retire the whole page, so the other threads keep
 * translating the page while it is retired under them (Muck's Mono JIT:
 * "translated blocks but none covered"). Each thread must always get the N
 * it just wrote. */
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#ifndef THREADS
#define THREADS 4
#endif
#ifndef ROUNDS
#define ROUNDS 300
#endif

static unsigned char *page;

static void *worker(void *arg) {
    long t = (long)arg;
    unsigned char *fn = page + 0x40 * t;
    long bad = 0;
    for (int i = 0; i < ROUNDS; i++) {
        int n = (int)(t * 100000 + i);
        unsigned char code[6] = {0xb8, 0, 0, 0, 0, 0xc3}; /* mov eax, n; ret */
        memcpy(code + 1, &n, 4);
        memcpy(fn, code, sizeof code);
        __asm__ volatile("" ::: "memory");
        int got = ((int (*)(void))fn)();
        if (got != n) bad++;
    }
    return (void *)bad;
}

int main(void) {
    page = mmap(0, 0x1000, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) return 2;
    pthread_t th[THREADS];
    for (long t = 0; t < THREADS; t++) pthread_create(&th[t], 0, worker, (void *)t);
    long bad = 0;
    for (int t = 0; t < THREADS; t++) {
        void *r;
        pthread_join(th[t], &r);
        bad += (long)r;
    }
    printf("%d threads, %d rounds each, %ld wrong results\n", THREADS, ROUNDS, bad);
    return bad != 0;
}
