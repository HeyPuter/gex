/* A JIT writing code while another thread translates and runs it: the writer
 * stores a new function (`mov eax, N; ret`, N new each round) over the old
 * one with a single aligned 8-byte store, while the caller keeps calling it,
 * so translations of the function race with the writes. After each round
 * both threads meet, and the call must return that round's N: a block
 * guard that recorded the bytes after decoding (newer than what the block
 * was translated from) would let a stale translation answer for ever. */
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>

/* Spin for about `us` microseconds: writes spaced out like this land while
 * the other thread is translating the function they rewrite. */
static void spin(long us) {
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    do clock_gettime(CLOCK_MONOTONIC, &b);
    while ((b.tv_sec - a.tv_sec) * 1000000L + (b.tv_nsec - a.tv_nsec) / 1000 < us);
}

#ifndef ROUNDS
#define ROUNDS 200
#endif
#ifndef CHURN
#define CHURN 20
#endif

static volatile uint64_t *fn;
static atomic_int round_no, done_writing;

static uint64_t code_for(int n) {
    unsigned char c[8] = {0xb8, 0, 0, 0, 0, 0xc3, 0xcc, 0xcc}; /* mov eax, n; ret */
    memcpy(c + 1, &n, 4);
    uint64_t w;
    memcpy(&w, c, 8);
    return w;
}

static void *writer(void *arg) {
    (void)arg;
    for (int r = 1; r <= ROUNDS; r++) {
        /* Churn: many versions while the caller is calling. */
        for (int k = 0; k < CHURN; k++) {
            __atomic_store_n(fn, code_for(r * 1000 + k), __ATOMIC_SEQ_CST);
            spin(100 + 37 * (k % 7));
        }
        __atomic_store_n(fn, code_for(r * 1000 + 999), __ATOMIC_SEQ_CST);
        spin(2000);
        atomic_store(&done_writing, r);
        while (atomic_load(&round_no) != r) {
        }
    }
    return 0;
}

int main(void) {
    unsigned char *page = mmap(0, 0x1000, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (page == MAP_FAILED) return 2;
    fn = (volatile uint64_t *)(page + 0x100);
    *fn = code_for(0);
    pthread_t w;
    pthread_create(&w, 0, writer, 0);
    long bad = 0, calls = 0;
    for (int r = 1; r <= ROUNDS; r++) {
        while (atomic_load(&done_writing) != r) {
            ((int (*)(void))(uintptr_t)fn)();
            calls++;
        }
        for (int k = 0; k < 3; k++) {
            int got = ((int (*)(void))(uintptr_t)fn)();
            if (got != r * 1000 + 999) bad++;
        }
        atomic_store(&round_no, r);
    }
    pthread_join(w, 0);
    (void)calls;
    printf("%d rounds, %ld stale results\n", ROUNDS, bad);
    return bad != 0;
}
