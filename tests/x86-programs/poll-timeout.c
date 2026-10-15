/* Timed waits that only their deadline ends: poll, ppoll, select and
 * epoll_wait on a pipe nobody writes, with a 100 ms timeout, in the main
 * thread, in a second thread and in a forked child.
 *
 * In the page such a wait gives the stack back (gex_syscall traps out to the
 * run loop), and the run loop asks again until the deadline. A trap skips
 * every Drop, and the clock scope the syscall had opened (`NowScope`) stayed
 * open on that Worker with its first reading: each later question got the
 * same time, the deadline never came, and the call never returned.
 *
 * Each call must return 0 after at least 90 ms and well inside 5 s. */
#define _GNU_SOURCE
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long long ms_now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1000LL + t.tv_nsec / 1000000;
}

static int bad;

static void check(const char *who, const char *what, int got, long long t0) {
    long long took = ms_now() - t0;
    if (got != 0 || took < 90 || took > 5000) {
        printf("%s: %s returned %d after %lld ms\n", who, what, got, took);
        bad = 1;
    }
}

static void waits(const char *who) {
    int p[2];
    if (pipe(p) != 0) {
        printf("%s: pipe failed\n", who);
        bad = 1;
        return;
    }
    long long t0 = ms_now();
    struct pollfd pf = {p[0], POLLIN, 0};
    check(who, "poll", poll(&pf, 1, 100), t0);

    t0 = ms_now();
    struct timespec ts = {0, 100000000};
    check(who, "ppoll", ppoll(&pf, 1, &ts, NULL), t0);

    t0 = ms_now();
    fd_set r;
    FD_ZERO(&r);
    FD_SET(p[0], &r);
    struct timeval tv = {0, 100000};
    check(who, "select", select(p[0] + 1, &r, NULL, NULL, &tv), t0);

    int e = epoll_create1(0);
    struct epoll_event out[4];
    t0 = ms_now();
    check(who, "epoll_wait (empty)", epoll_wait(e, out, 4, 100), t0);
    struct epoll_event ev = {.events = EPOLLIN, .data.fd = p[0]};
    epoll_ctl(e, EPOLL_CTL_ADD, p[0], &ev);
    t0 = ms_now();
    check(who, "epoll_wait (pipe)", epoll_wait(e, out, 4, 100), t0);
    close(e);
    close(p[0]);
    close(p[1]);
}

static void *thread_main(void *arg) {
    (void)arg;
    waits("thread");
    return NULL;
}

int main(void) {
    waits("main");

    pthread_t t;
    pthread_create(&t, NULL, thread_main, NULL);
    pthread_join(t, NULL);

    pid_t kid = fork();
    if (kid == 0) {
        waits("child");
        fflush(stdout);
        _exit(bad);
    }
    int st = 0;
    waitpid(kid, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) bad = 1;

    printf("poll-timeout %s\n", bad ? "FAILED" : "ok");
    return bad;
}
