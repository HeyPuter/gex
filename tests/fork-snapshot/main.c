/* tests/fork-snapshot/run.sh's program: a fork copies the parent's memory
 * at one moment, as Linux does, while its other threads run on. Threads
 * write B = k, then A = k, for k = 1, 2, ...; A is in .bss and B in a
 * mapping far above it, so a copy made a piece at a time while they run
 * (A's page early, B's later) gives the child an A well behind its B. At any
 * one moment A is B or B - 1, and the child says which it saw.
 *
 *   main FORKS THREADS: prints "forks N torn T" and exits 0 when T is 0. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define MAXT 8
static volatile uint64_t A[MAXT];
static volatile uint64_t *B;
static volatile int stop;

static void *writer(void *arg)
{
    int t = (int)(intptr_t)arg;
    for (uint64_t k = 1; !stop; k++) {
        B[t * 512] = k;
        __atomic_thread_fence(__ATOMIC_RELEASE);
        A[t] = k;
        __atomic_thread_fence(__ATOMIC_RELEASE);
    }
    return NULL;
}

int main(int argc, char **argv)
{
    int forks = argc > 1 ? atoi(argv[1]) : 10;
    int threads = argc > 2 ? atoi(argv[2]) : 2;
    if (threads > MAXT) threads = MAXT;
    /* Far above .bss: 64 MiB of mapping, B at its end. */
    size_t len = 64u << 20;
    char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) return 2;
    B = (volatile uint64_t *)(m + len - MAXT * 512 * 8);
    pthread_t th[MAXT];
    for (int t = 0; t < threads; t++) pthread_create(&th[t], NULL, writer, (void *)(intptr_t)t);
    while (A[threads - 1] < 1000) usleep(1000);
    int torn = 0;
    for (int i = 0; i < forks; i++) {
        pid_t p = fork();
        if (p == 0) {
            int bad = 0;
            for (int t = 0; t < threads; t++) {
                uint64_t a = A[t], b = B[t * 512];
                if (b - a > 1) {
                    printf("child %d: thread %d A=%llu B=%llu\n", i, t, (unsigned long long)a, (unsigned long long)b);
                    bad = 1;
                }
            }
            fflush(stdout);
            _exit(bad);
        }
        int st = 0;
        if (p < 0 || waitpid(p, &st, 0) != p || !WIFEXITED(st)) return 3;
        torn += WEXITSTATUS(st) != 0;
    }
    stop = 1;
    for (int t = 0; t < threads; t++) pthread_join(th[t], NULL);
    printf("forks %d torn %d\n", forks, torn);
    return torn ? 1 : 0;
}
