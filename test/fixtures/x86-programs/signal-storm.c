/* Signals delivered by the threads themselves, with no kernel lock: every thread asks its own pending word at
 * its safepoints and builds its own handler frame.
 *
 * - targeted: a storm of pthread_kill to spinning threads (no syscalls in
 *   their loops, so only a safepoint sees the signal): each handler runs on
 *   the thread it was sent to, and every thread takes at least one;
 * - once: a process-directed signal, raised one at a time while four
 *   threads spin, is taken by exactly one of them, never two;
 * - masked: a process-directed signal is never taken by a thread that
 *   blocks it, while one that does not takes them all. */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define N 4

static pthread_t th[N];
static atomic_int stop, up;
static atomic_long taken[N], wrong, once_taken, masked_wrong;
static _Thread_local int me = -1;
static _Thread_local int blocks_usr2;

static void on_usr1(int s) {
    (void)s;
    if (me < 0 || !pthread_equal(pthread_self(), th[me]))
        atomic_fetch_add(&wrong, 1);
    else
        atomic_fetch_add(&taken[me], 1);
}

static void on_usr2(int s) {
    (void)s;
    if (blocks_usr2) atomic_fetch_add(&masked_wrong, 1);
    atomic_fetch_add(&once_taken, 1);
}

static void *spinner(void *arg) {
    me = (int)(long)arg;
    /* Odd threads keep SIGUSR2 blocked, as the main thread created them
       with it; even ones take it. */
    if (me & 1) {
        blocks_usr2 = 1;
    } else {
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGUSR2);
        pthread_sigmask(SIG_UNBLOCK, &s, NULL);
    }
    atomic_fetch_add(&up, 1);
    volatile unsigned long x = 0;
    while (!atomic_load(&stop)) x++;
    return 0;
}

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    sa.sa_handler = on_usr2;
    sigaction(SIGUSR2, &sa, NULL);
    /* The main thread takes no SIGUSR2: only the spinners compete. */
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR2);
    pthread_sigmask(SIG_BLOCK, &s, NULL);
    for (long i = 0; i < N; i++) pthread_create(&th[i], 0, spinner, (void *)i);
    while (atomic_load(&up) < N) usleep(1000);

    /* targeted */
    for (int round = 0; round < 400; round++) {
        pthread_kill(th[round % N], SIGUSR1);
        if (round % 16 == 0) usleep(200);
    }
    double end = now() + 20;
    int all = 0;
    while (!all && now() < end) {
        all = 1;
        for (int i = 0; i < N; i++)
            if (!atomic_load(&taken[i])) all = 0;
        usleep(1000);
    }
    printf("targeted: %s\n", atomic_load(&wrong) ? "a handler ran on another thread"
                             : all ? "ok" : "a thread never took its signal");

    /* once */
    int twice = 0, lost = 0;
    for (int i = 0; i < 200; i++) {
        long before = atomic_load(&once_taken);
        kill(getpid(), SIGUSR2);
        end = now() + 10;
        while (atomic_load(&once_taken) == before && now() < end) usleep(100);
        if (atomic_load(&once_taken) == before) { lost = 1; break; }
        usleep(500);
        if (atomic_load(&once_taken) != before + 1) { twice = 1; break; }
    }
    printf("once: %s\n", twice ? "a signal was taken twice" : lost ? "a signal was never taken" : "ok");
    printf("masked: %s\n", atomic_load(&masked_wrong) ? "a blocking thread took one" : "ok");

    atomic_store(&stop, 1);
    for (int i = 0; i < N; i++) pthread_join(th[i], 0);
    return 0;
}
