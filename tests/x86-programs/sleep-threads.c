/* Sleeps from several threads at once (docs/kernel-locking.md §5.2 step 5,
 * nanosleep without the kernel lock): each lasts at least what it asked,
 * an absolute clock_nanosleep wakes at its deadline, a deadline already
 * past returns at once, a malformed request is EINVAL, and a signal sent by
 * another thread ends a sleep with EINTR after its handler ran. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>

static long ms_since(const struct timespec *a) {
    struct timespec b;
    clock_gettime(CLOCK_MONOTONIC, &b);
    return (b.tv_sec - a->tv_sec) * 1000 + (b.tv_nsec - a->tv_nsec) / 1000000;
}

static int short_ok[4];
static void *sleeper(void *arg) {
    long me = (long)arg;
    int ok = 1;
    for (int i = 0; i < 20; i++) {
        struct timespec t0, req = {0, 2000000 * (me + 1)};
        clock_gettime(CLOCK_MONOTONIC, &t0);
        if (nanosleep(&req, NULL) != 0 || ms_since(&t0) < 2 * (me + 1)) ok = 0;
    }
    short_ok[me] = ok;
    return NULL;
}

static volatile sig_atomic_t handled;
static void on_usr1(int s) { (void)s; handled = 1; }
static pthread_t target;
static void *poker(void *arg) {
    (void)arg;
    usleep(50000);
    pthread_kill(target, SIGUSR1);
    return NULL;
}

int main(void) {
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], NULL, sleeper, (void *)i);
    for (int i = 0; i < 4; i++) pthread_join(t[i], NULL);
    printf("sleeps last what they asked: %s\n", short_ok[0] && short_ok[1] && short_ok[2] && short_ok[3] ? "yes" : "no");

    struct timespec t0, at;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    at = t0;
    at.tv_nsec += 30000000;
    if (at.tv_nsec >= 1000000000) at.tv_sec++, at.tv_nsec -= 1000000000;
    int r = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &at, NULL);
    long took = ms_since(&t0);
    printf("absolute sleep: %d, on time: %s\n", r, took >= 30 && took < 2000 ? "yes" : "no");
    clock_gettime(CLOCK_MONOTONIC, &t0);
    at = t0;
    at.tv_sec -= 1;
    r = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &at, NULL);
    printf("past deadline: %d, at once: %s\n", r, ms_since(&t0) < 1000 ? "yes" : "no");
    struct timespec bad = {0, 1000000000};
    printf("malformed: %s\n", nanosleep(&bad, NULL) == -1 && errno == EINVAL ? "EINVAL" : "other");

    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    target = pthread_self();
    pthread_t p;
    pthread_create(&p, NULL, poker, NULL);
    struct timespec longreq = {5, 0};
    clock_gettime(CLOCK_MONOTONIC, &t0);
    r = nanosleep(&longreq, NULL);
    int e = errno;
    pthread_join(p, NULL);
    printf("signalled sleep: %s, handler ran: %s, early: %s\n", r == -1 && e == EINTR ? "EINTR" : "other",
           handled ? "yes" : "no", ms_since(&t0) < 4000 ? "yes" : "no");
    return 0;
}
