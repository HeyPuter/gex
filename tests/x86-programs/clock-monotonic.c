/* CLOCK_MONOTONIC as a program relies on it: never going back, in one thread
 * or between two (in the page each thread is its own Worker, with its own
 * clock origin), finer than a millisecond (delta-time code divides by it;
 * Wine's QueryPerformanceCounter sits on it), and a sleep lasting at least
 * what it asked. The page read Date.now(): 1 ms steps that follow the wall
 * clock, backwards included. */
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>

#define N 200000

static int64_t mono(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static _Atomic int64_t published;
static atomic_int behind, done;

/* Publishes its readings; the main thread's reading after seeing one must
 * not be earlier than it. */
static void *writer(void *arg) {
    (void)arg;
    while (!atomic_load(&done)) atomic_store(&published, mono());
    return 0;
}

int main(void) {
    int64_t prev = mono(), min_step = INT64_MAX;
    int back = 0;
    for (int i = 0; i < N; i++) {
        int64_t t = mono();
        if (t < prev) back++;
        else if (t > prev && t - prev < min_step) min_step = t - prev;
        prev = t;
    }
    printf("one thread never goes back: %s\n", back == 0 ? "yes" : "no");
    printf("finer than a millisecond: %s\n", min_step < 1000000 ? "yes" : "no");

    pthread_t th;
    pthread_create(&th, 0, writer, 0);
    while (atomic_load(&published) == 0) ;
    for (int i = 0; i < N; i++) {
        int64_t seen = atomic_load(&published);
        if (mono() < seen) atomic_fetch_add(&behind, 1);
    }
    atomic_store(&done, 1);
    pthread_join(th, 0);
    printf("never behind another thread: %s\n", atomic_load(&behind) == 0 ? "yes" : "no");

    struct timespec two = {0, 2000000};
    int64_t a = mono();
    nanosleep(&two, 0);
    int64_t b = mono();
    printf("a 2 ms sleep lasts at least 2 ms: %s\n", b - a >= 2000000 ? "yes" : "no");
    return 0;
}
