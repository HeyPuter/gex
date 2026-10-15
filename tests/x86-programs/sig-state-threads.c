/* The calls on a process's and a thread's own signal state, and getrandom,
 * made without the kernel lock (docs/kernel-locking.md, L15), from several
 * threads at once. Each thread installs and reads back its own signal's
 * handler (sigaction: the process's dispositions, shared), keeps an
 * alternate stack of its own (sigaltstack: the thread's), sees a signal it
 * blocked and raised as pending (sigpending), and draws random bytes
 * (getrandom). Meanwhile the main thread runs an interval timer
 * (setitimer, then alarm) whose SIGALRMs are counted, and reads the time
 * left (getitimer). */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/random.h>
#include <sys/time.h>
#include <unistd.h>

#define THREADS 4
#define ROUNDS 2000
static long bad;
static volatile int alarms;

static void fail(const char *what) {
    if (__atomic_add_fetch(&bad, 1, __ATOMIC_RELAXED) < 5) fprintf(stderr, "bad: %s (errno %d)\n", what, errno);
}

static void on_alarm(int s) { (void)s; alarms++; }
static void h0(int s) { (void)s; }
static void h1(int s) { (void)s; }
static void (*const handlers[2])(int) = {h0, h1};

static void *worker(void *arg) {
    long i = (long)arg;
    int signo = SIGRTMIN + 1 + (int)i;
    static char stacks[THREADS][65536];
    stack_t ss = {.ss_sp = stacks[i], .ss_size = sizeof stacks[i], .ss_flags = 0};
    if (sigaltstack(&ss, NULL)) fail("sigaltstack set");
    for (int r = 0; r < ROUNDS; r++) {
        struct sigaction sa = {0}, old;
        sa.sa_handler = handlers[r & 1];
        sigemptyset(&sa.sa_mask);
        sigaddset(&sa.sa_mask, SIGUSR2);
        if (sigaction(signo, &sa, NULL)) fail("sigaction set");
        if (sigaction(signo, NULL, &old)) fail("sigaction get");
        /* Only this thread sets this signal's handler. */
        if (old.sa_handler != handlers[r & 1] || !sigismember(&old.sa_mask, SIGUSR2)) fail("another handler read back");
        stack_t cur;
        if (sigaltstack(NULL, &cur) || cur.ss_sp != stacks[i] || cur.ss_size != sizeof stacks[i]) fail("another thread's alternate stack");
        unsigned char b[32];
        if (getrandom(b, sizeof b, 0) != sizeof b) fail("getrandom");
    }
    /* A signal of its own, blocked and raised, is pending, then taken. */
    sigset_t block, pend;
    sigemptyset(&block);
    sigaddset(&block, signo);
    pthread_sigmask(SIG_BLOCK, &block, NULL);
    raise(signo);
    if (sigpending(&pend) || !sigismember(&pend, signo)) fail("sigpending");
    struct sigaction ign = {0};
    ign.sa_handler = h0;
    sigaction(signo, &ign, NULL);
    pthread_sigmask(SIG_UNBLOCK, &block, NULL);
    return NULL;
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_alarm;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGALRM, &sa, NULL);
    struct itimerval it = {{0, 2000}, {0, 2000}}, left;
    if (setitimer(ITIMER_REAL, &it, NULL)) fail("setitimer");
    pthread_t t[THREADS];
    for (long i = 0; i < THREADS; i++) pthread_create(&t[i], NULL, worker, (void *)i);
    for (int i = 0; i < THREADS; i++) pthread_join(t[i], NULL);
    while (alarms < 3) {
        if (getitimer(ITIMER_REAL, &left)) { fail("getitimer"); break; }
        usleep(1000);
    }
    struct itimerval off = {{0, 0}, {0, 0}};
    if (setitimer(ITIMER_REAL, &off, &left)) fail("setitimer off");
    if (left.it_interval.tv_usec != 2000) fail("the interval read back");
    /* alarm: none left after the disarm; then 30 s, reported back. */
    if (alarm(30) != 0) fail("alarm: one left after the disarm");
    if (alarm(0) != 30) fail("alarm: the seconds left");
    printf("bad %ld\n", bad);
    return bad != 0;
}
