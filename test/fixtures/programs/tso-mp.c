/* tso: the guest of test/page/tso.test.js. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#define N 4096
/* One cache line per slot; volatile so the guest compiler keeps the order
 * (x86 compiles each to a plain mov). */
static volatile int data[N * 16], flag[N * 16];
static int go, done_;
static int rounds;
static long bad, seen;
static void barrier(int *w, int target) {
    __atomic_add_fetch(w, 1, __ATOMIC_SEQ_CST);
    while (__atomic_load_n(w, __ATOMIC_SEQ_CST) < target) { }
}
static void *writer(void *_) {
    for (int r = 0; r < rounds; r++) {
        int tag = r + 1;
        barrier(&go, 2 * (r + 1));
        for (int i = 0; i < N; i++) { data[i * 16] = tag; flag[i * 16] = tag; }
        barrier(&done_, 2 * (r + 1));
    }
    return 0;
}
int main(int argc, char **argv) {
    rounds = atoi(argv[1]);
    pthread_t t;
    pthread_create(&t, 0, writer, 0);
    for (int r = 0; r < rounds; r++) {
        int tag = r + 1;
        barrier(&go, 2 * (r + 1));
        for (int i = 0; i < N; i++) {
            int f = flag[i * 16];
            int d = data[i * 16];
            if (f == tag) { seen++; if (d != tag) bad++; }
        }
        barrier(&done_, 2 * (r + 1));
    }
    pthread_join(t, 0);
    printf("mp %ld of %ld\n", bad, seen);
    return 0;
}
