/* tests/fork-snapshot/run.sh's program: a fork copies the parent's memory
 * at one moment, as Linux does, while its other threads run on. Threads
 * write B = k, then A = k, for k = 1, 2, ...; A is in .bss and B in a
 * mapping far above it, so a copy made a piece at a time while they run
 * (A's page early, B's later) gives the child an A well behind its B. At any
 * one moment A is B or B - 1, and the child says which it saw.
 *
 * Writer 0 never makes a syscall: only its back-edges' safepoints can stop
 * it. With CALLS, the others make one every 64 writes. With CHURN, one more
 * thread creates and joins threads all along, so libc's list of threads
 * changes under the fork, and each child starts and joins one of its own.
 *
 *   main FORKS THREADS [CALLS [CHURN]]: prints "forks N torn T", then
 *   "fork ms M" (the mean from fork to the child's reaping), and exits 0
 *   when T is 0. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MAXT 8
static volatile uint64_t A[MAXT];
static volatile uint64_t *B;
static volatile int stop;
static int calls;

static void *writer(void *arg)
{
    int t = (int)(intptr_t)arg;
    int call = calls && t != 0;
    for (uint64_t k = 1; !stop; k++) {
        B[t * 512] = k;
        __atomic_thread_fence(__ATOMIC_RELEASE);
        A[t] = k;
        __atomic_thread_fence(__ATOMIC_RELEASE);
        if (call && (k & 63) == 0) syscall(SYS_sched_yield);
    }
    return NULL;
}

static void *nothing(void *arg)
{
    return arg;
}

static volatile uint64_t churned;

static void *churn(void *arg)
{
    (void)arg;
    while (!stop) {
        pthread_t t;
        if (pthread_create(&t, NULL, nothing, NULL) == 0) pthread_join(t, NULL);
        churned++;
    }
    return NULL;
}

static double now_ms(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}

int main(int argc, char **argv)
{
    int forks = argc > 1 ? atoi(argv[1]) : 10;
    int threads = argc > 2 ? atoi(argv[2]) : 2;
    calls = argc > 3 && atoi(argv[3]);
    int churning = argc > 4 && atoi(argv[4]);
    if (threads < 1) threads = 1;
    if (threads > MAXT) threads = MAXT;
    /* Far above .bss: 64 MiB of mapping, B at its end. */
    size_t len = 64u << 20;
    char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) return 2;
    B = (volatile uint64_t *)(m + len - MAXT * 512 * 8);
    pthread_t th[MAXT];
    pthread_t ch;
    for (int t = 0; t < threads; t++) pthread_create(&th[t], NULL, writer, (void *)(intptr_t)t);
    if (churning) pthread_create(&ch, NULL, churn, NULL);
    while (A[threads - 1] < 1000 || (churning && churned < 10)) usleep(1000);
    int torn = 0;
    double ms = 0;
    for (int i = 0; i < forks; i++) {
        double t0 = now_ms();
        pid_t p = fork();
        if (p == 0) {
            int bad = 0;
            if (churning) {
                pthread_t t;
                if (pthread_create(&t, NULL, nothing, NULL) != 0 || pthread_join(t, NULL) != 0) {
                    printf("child %d: no thread\n", i);
                    bad = 1;
                }
            }
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
        ms += now_ms() - t0;
        torn += WEXITSTATUS(st) != 0;
    }
    stop = 1;
    for (int t = 0; t < threads; t++) pthread_join(th[t], NULL);
    if (churning) pthread_join(ch, NULL);
    printf("forks %d torn %d\n", forks, torn);
    printf("fork ms %.1f\n", ms / forks);
    return torn ? 1 : 0;
}
