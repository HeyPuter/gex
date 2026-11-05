/* rt_sigtimedwait that waits (it had answered EAGAIN at once, whatever the
 * timeout). Every thread blocks SIGUSR1; waiters take it with
 * sigwaitinfo/sigtimedwait while it is sent from elsewhere.
 *
 * - A waiter in sigwaitinfo (no timeout) takes thirty SIGUSR1s that main
 *   sends it with tgkill, each after the waiter is already waiting.
 * - Two waiters in sigtimedwait (200 ms at a time) take twenty process
 *   SIGUSR1s that main sends with kill: each signal is taken exactly once.
 * - A sigtimedwait for 50 ms with nothing sent says EAGAIN, and not before
 *   most of the time has passed.
 * - A waiter for SIGUSR1 (the raw call) is interrupted by a SIGUSR2 that
 *   has a handler (EINTR; the handler ran).
 * - A child sends its parent SIGUSR1 while the parent waits in
 *   sigwaitinfo: the parent takes it, and si_pid names the child. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define DIRECT 30
#define SHARED 20

static atomic_int bad;
static atomic_int waiter_tid, waiting, got_direct, got_shared, handled, intr_tid, intr_waiting, stop;

static void fail(const char *what, long n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%ld, errno %d)\n", what, n, errno);
}

static int gettid_(void) { return (int)syscall(SYS_gettid); }
static int tgkill_(int tgid, int tid, int sig) { return (int)syscall(SYS_tgkill, tgid, tid, sig); }

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void on_usr2(int sig) { (void)sig; atomic_fetch_add(&handled, 1); }

static sigset_t usr1(void) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    return s;
}

/* Takes DIRECT thread-directed SIGUSR1s with sigwaitinfo. */
static void *direct(void *arg) {
    (void)arg;
    sigset_t s = usr1();
    atomic_store(&waiter_tid, gettid_());
    for (int i = 0; i < DIRECT; i++) {
        siginfo_t si;
        atomic_store(&waiting, i + 1);
        int got = sigwaitinfo(&s, &si);
        if (got != SIGUSR1) fail("sigwaitinfo", got);
        else if (si.si_code != SI_TKILL || si.si_pid != getpid()) fail("tgkill's siginfo", si.si_code);
        atomic_fetch_add(&got_direct, 1);
    }
    return 0;
}

/* Takes process SIGUSR1s with sigtimedwait until told to stop; a wait that
 * times out (200 ms) just waits again. */
static void *shared(void *arg) {
    (void)arg;
    sigset_t s = usr1();
    struct timespec ts = {0, 200000000};
    while (!atomic_load(&stop)) {
        siginfo_t si;
        int got = sigtimedwait(&s, &si, &ts);
        if (got == SIGUSR1) {
            if (si.si_code != SI_USER || si.si_pid != getpid()) fail("kill's siginfo", si.si_code);
            atomic_fetch_add(&got_shared, 1);
        } else if (!(got < 0 && errno == EAGAIN)) {
            fail("sigtimedwait", got);
        }
    }
    return 0;
}

/* Waits for SIGUSR1 and is interrupted by SIGUSR2 instead. The raw call:
 * musl's sigtimedwait makes it again after EINTR. Its timespec is the
 * call's own (two longs: 32-bit on i386, as SYS_rt_sigtimedwait wants). */
static void *interrupted(void *arg) {
    (void)arg;
    sigset_t s = usr1();
    struct { long s, ns; } ts = {2, 0};
    atomic_store(&intr_tid, gettid_());
    atomic_store(&intr_waiting, 1);
    errno = 0;
    long got = syscall(SYS_rt_sigtimedwait, &s, 0, &ts, 8);
    if (got != -1 || errno != EINTR) fail("sigtimedwait interrupted", got);
    return 0;
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    sigset_t s = usr1();
    pthread_sigmask(SIG_BLOCK, &s, 0);
    struct sigaction sa = {0};
    sa.sa_handler = on_usr2;
    sigaction(SIGUSR2, &sa, 0);

    /* Thread-directed, one at a time, each to a waiter already waiting. */
    pthread_t t;
    pthread_create(&t, 0, direct, 0);
    for (int i = 0; i < DIRECT; i++) {
        while (atomic_load(&waiting) <= i || atomic_load(&got_direct) < i) sched_yield();
        usleep(1000);
        if (tgkill_(getpid(), atomic_load(&waiter_tid), SIGUSR1)) fail("tgkill", i);
    }
    pthread_join(t, 0);

    /* Process-directed, to two waiters: each taken once. */
    pthread_t w[2];
    for (int i = 0; i < 2; i++) pthread_create(&w[i], 0, shared, 0);
    for (int i = 0; i < SHARED; i++) {
        usleep(2000);
        if (kill(getpid(), SIGUSR1)) fail("kill", i);
        /* Two pending would be one: the next waits for this one's taking. */
        while (atomic_load(&got_shared) <= i) sched_yield();
    }
    atomic_store(&stop, 1);
    for (int i = 0; i < 2; i++) pthread_join(w[i], 0);

    /* Nothing sent: EAGAIN after the time. */
    double t0 = now();
    errno = 0;
    int r = sigtimedwait(&s, 0, &(struct timespec){0, 50000000});
    double dt = now() - t0;
    if (r != -1 || errno != EAGAIN) fail("timeout's answer", r);
    if (dt < 0.04) fail("timed out early (ms)", (long)(dt * 1000));

    /* Interrupted by a handled signal outside the set. */
    pthread_create(&t, 0, interrupted, 0);
    while (!atomic_load(&intr_waiting)) sched_yield();
    usleep(20000);
    if (tgkill_(getpid(), atomic_load(&intr_tid), SIGUSR2)) fail("tgkill SIGUSR2", 0);
    pthread_join(t, 0);
    if (atomic_load(&handled) != 1) fail("handler runs", atomic_load(&handled));

    /* From a child, while the parent waits. */
    pid_t c = fork();
    if (c == 0) {
        usleep(20000);
        kill(getppid(), SIGUSR1);
        _exit(0);
    }
    siginfo_t si;
    int got = sigwaitinfo(&s, &si);
    if (got != SIGUSR1) fail("sigwaitinfo from a child", got);
    else if (si.si_pid != c || si.si_code != SI_USER) fail("child's siginfo", si.si_code);
    int st;
    waitpid(c, &st, 0);

    printf("direct %d shared %d bad %d\n", atomic_load(&got_direct), atomic_load(&got_shared), atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
