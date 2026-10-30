/* thread-signal: the guest of test/page/thread-signal.test.js. */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>
static volatile sig_atomic_t hit, rang, ready;
static volatile unsigned long sink;
static void on_usr1(int s) { (void)s; hit = 1; }
static void on_alrm(int s) { (void)s; rang = 1; }
static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}
/* No syscall in the loop: only a safepoint can stop it. */
static void *spin(void *a) {
    (void)a;
    ready = 1;
    unsigned long h = 1;
    while (!hit) { h = h * 6364136223846793005UL + 1; sink = h; }
    return 0;
}
int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    signal(SIGUSR1, on_usr1);
    signal(SIGALRM, on_alrm);
    int late = 0;
    for (int round = 0; round < 5; round++) {
        hit = 0;
        ready = 0;
        pthread_t t;
        pthread_create(&t, 0, spin, 0);
        while (!ready) usleep(1000);
        usleep(20000);
        double t0 = now();
        pthread_kill(t, SIGUSR1);
        pthread_join(t, 0);
        if (now() - t0 >= 1.0) late++;
    }
    printf("spinning threads took SIGUSR1 promptly: %s\n", late ? "no" : "yes");
    alarm(1);
    double t0 = now();
    unsigned long h = 1;
    while (!rang && now() - t0 < 10) {
        for (int i = 0; i < 1000; i++) { h = h * 6364136223846793005UL + 1; sink = h; }
    }
    printf("alarm ended a spin: %s\n", rang ? "yes" : "no");
    return 0;
}
