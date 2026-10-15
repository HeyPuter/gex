/* Signals sent between threads and processes from several threads at once
 * (L22d: kill, tkill and tgkill without the kernel lock).
 *
 * - Four threads pass a token round a ring with tgkill: each waits for its
 *   turn, then signals the next thread, whose handler counts it.
 * - One thread blocks in a read of an empty pipe and is interrupted by a
 *   tgkill from main (a handler without SA_RESTART: EINTR).
 * - Main sends its own process SIGUSR2 with kill, ten times. Every thread
 *   blocks it, and a waiter takes each with sigtimedwait (asked with no
 *   wait, until it comes) and checks the sender.
 * - Another thread waits in sigsuspend with SIGURG let through, twenty
 *   times, woken each time by main's tgkill: the handler has run, the call
 *   says EINTR, and SIGURG is blocked again after.
 * - Main forks a child that waits for SIGTERM, sends it with kill, and
 *   checks the child's status.
 * - tgkill to a thread of a process that does not own it is ESRCH, and a
 *   probe (signal 0) of a live thread is 0. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define RING 4
#define LAPS 50

static atomic_int bad;
static atomic_int tids[RING + 2];
static atomic_int got[RING];
static atomic_int turn, taken;
static int idle[2];

static void fail(const char *what, long n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%ld, errno %d)\n", what, n, errno);
}

static int gettid_(void) { return (int)syscall(SYS_gettid); }
static int tgkill_(int tgid, int tid, int sig) { return (int)syscall(SYS_tgkill, tgid, tid, sig); }

static void on_usr1(int sig) {
    (void)sig;
    int me = gettid_();
    for (int i = 0; i < RING; i++)
        if (atomic_load(&tids[i]) == me) atomic_fetch_add(&got[i], 1);
}

static void on_alrm(int sig) { (void)sig; }

static void *ring(void *arg) {
    int id = (int)(long)arg;
    atomic_store(&tids[id], gettid_());
    for (int i = 0; i < RING; i++)
        while (atomic_load(&tids[i]) == 0) sched_yield();
    for (int lap = 0; lap < LAPS; lap++) {
        int mine = lap * RING + id;
        /* Wait for the turn, and for the signal that gave it (none for the
         * very first). */
        while (atomic_load(&turn) != mine) sched_yield();
        if (mine > 0) {
            struct timespec t0;
            clock_gettime(CLOCK_MONOTONIC, &t0);
            while (atomic_load(&got[id]) < lap + (id > 0 ? 1 : 0)) {
                struct timespec t;
                clock_gettime(CLOCK_MONOTONIC, &t);
                if (t.tv_sec - t0.tv_sec > 5) { fail("token's signal never came", mine); break; }
                sched_yield();
            }
        }
        int next = (id + 1) % RING;
        atomic_store(&turn, mine + 1);
        if (mine + 1 < LAPS * RING && tgkill_(getpid(), atomic_load(&tids[next]), SIGUSR1)) fail("tgkill", mine);
    }
    return 0;
}

static void *reader(void *arg) {
    (void)arg;
    atomic_store(&tids[RING], gettid_());
    char c;
    errno = 0;
    ssize_t n = read(idle[0], &c, 1);
    if (n != -1 || errno != EINTR) fail("read not interrupted", (long)n);
    return 0;
}

static void *waiter(void *arg) {
    (void)arg;
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    for (int i = 0; i < 10; i++) {
        /* Asked with no wait, again until it comes (5 s at most). */
        siginfo_t si;
        struct timespec zero = { 0, 0 }, t0, t;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        int got = -1;
        while ((got = sigtimedwait(&set, &si, &zero)) < 0) {
            clock_gettime(CLOCK_MONOTONIC, &t);
            if (errno != EAGAIN || t.tv_sec - t0.tv_sec > 5) break;
            sched_yield();
        }
        if (got != SIGUSR2) fail("sigtimedwait", i);
        else if (si.si_pid != getpid() || si.si_code != SI_USER) fail("SIGUSR2's sender", si.si_pid);
        atomic_fetch_add(&taken, 1);
    }
    return 0;
}

static atomic_int urg, suspended;

static void on_urg(int sig) { (void)sig; atomic_fetch_add(&urg, 1); }

static void *suspender(void *arg) {
    (void)arg;
    atomic_store(&tids[RING + 1], gettid_());
    sigset_t urgent, during;
    sigemptyset(&urgent);
    sigaddset(&urgent, SIGURG);
    pthread_sigmask(SIG_BLOCK, &urgent, 0);
    pthread_sigmask(SIG_SETMASK, 0, &during);
    sigdelset(&during, SIGURG);
    for (int i = 0; i < 20; i++) {
        int before = atomic_load(&urg);
        atomic_store(&suspended, i + 1);
        errno = 0;
        if (sigsuspend(&during) != -1 || errno != EINTR) fail("sigsuspend not EINTR", i);
        if (atomic_load(&urg) != before + 1) fail("SIGURG's handler", atomic_load(&urg));
        sigset_t now;
        pthread_sigmask(SIG_SETMASK, 0, &now);
        if (!sigismember(&now, SIGURG)) fail("mask not put back", i);
    }
    return 0;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &sa, 0);
    sa.sa_handler = on_alrm;
    sa.sa_flags = 0;
    sigaction(SIGALRM, &sa, 0);
    sa.sa_handler = on_urg;
    sigaction(SIGURG, &sa, 0);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR2);
    pthread_sigmask(SIG_BLOCK, &set, 0);
    if (pipe(idle)) return 2;

    pthread_t t[RING + 3];
    for (int i = 0; i < RING; i++) pthread_create(&t[i], 0, ring, (void *)(long)i);
    pthread_create(&t[RING], 0, reader, 0);
    pthread_create(&t[RING + 1], 0, waiter, 0);
    pthread_create(&t[RING + 2], 0, suspender, 0);

    /* The suspender, woken twenty times. */
    for (int i = 1; i <= 20; i++) {
        while (atomic_load(&suspended) < i) sched_yield();
        usleep(1000);
        if (tgkill_(getpid(), atomic_load(&tids[RING + 1]), SIGURG)) fail("tgkill suspender", i);
    }

    /* The reader, parked in its read, interrupted. */
    while (atomic_load(&tids[RING]) == 0) sched_yield();
    usleep(20000);
    if (tgkill_(getpid(), atomic_load(&tids[RING]), 0)) fail("probe of a live thread", 0);
    if (tgkill_(getpid(), atomic_load(&tids[RING]), SIGALRM)) fail("tgkill reader", 0);

    /* The process's own SIGUSR2, ten times, each taken by the waiter
     * before the next is sent (two pending would be one). */
    for (int i = 0; i < 10; i++) {
        if (kill(getpid(), SIGUSR2)) fail("kill self", i);
        while (atomic_load(&taken) <= i) sched_yield();
    }

    /* A child, ended by kill. */
    pid_t c = fork();
    if (c == 0) {
        for (;;) pause();
    }
    usleep(20000);
    if (kill(c, SIGTERM)) fail("kill child", c);
    int st = 0;
    if (waitpid(c, &st, 0) != c || !WIFSIGNALED(st) || WTERMSIG(st) != SIGTERM) fail("child's end", st);
    /* Its thread is not this process's. */
    errno = 0;
    if (tgkill_(getpid(), c, SIGUSR1) != -1 || errno != ESRCH) fail("tgkill to a gone task", errno);

    for (int i = 0; i < RING + 3; i++) pthread_join(t[i], 0);
    int total = 0;
    for (int i = 0; i < RING; i++) total += atomic_load(&got[i]);
    if (total != LAPS * RING - 1) fail("ring signals", total);
    printf("laps %d bad %d\n", LAPS, atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
