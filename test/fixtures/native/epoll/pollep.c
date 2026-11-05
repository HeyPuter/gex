/* An epoll descriptor in a poll set, as GLib's main loop polls a library's
 * own epoll: readable only while epoll_wait would report something. With
 * nothing ready, ppoll on it must block until its timeout (a loop around it
 * otherwise spins a core); written, it wakes with POLLIN and never POLLOUT.
 * Also a set inside a set, and a fork's copy of the set. */
#define _GNU_SOURCE
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* ppoll on fd for POLLIN|POLLOUT for up to ms: what it returned, its revents,
 * and whether it waited out its time or came back at once. */
static void show(const char *what, int fd, int ms) {
    struct pollfd p = { fd, POLLIN | POLLOUT, 0 };
    struct timespec t = { ms / 1000, (ms % 1000) * 1000000L };
    double t0 = now();
    int n = ppoll(&p, 1, &t, NULL);
    double dt = (now() - t0) * 1000;
    printf("%s: %d revents=%s%s %s\n", what, n, p.revents & POLLIN ? "IN" : "-", p.revents & POLLOUT ? "+OUT" : "",
           n == 0 ? (dt >= ms * 0.8 ? "waited" : "returned early") : (dt < ms * 0.8 ? "woke" : "late"));
}

int main(void) {
    int pfd[2], ep = epoll_create1(0), outer = epoll_create1(0);
    char c;
    if (pipe(pfd) || ep < 0 || outer < 0) { perror("setup"); return 1; }
    show("empty set", ep, 300);
    struct epoll_event e = { .events = EPOLLIN, .data.u64 = 7 };
    epoll_ctl(ep, EPOLL_CTL_ADD, pfd[0], &e);
    show("empty pipe", ep, 300);
    epoll_ctl(outer, EPOLL_CTL_ADD, ep, &e);
    show("empty pipe, two sets deep", outer, 300);
    /* a GLib-style loop: 10 rounds of 50 ms, each must wait */
    int early = 0;
    for (int i = 0; i < 10; i++) {
        struct pollfd p = { ep, POLLIN, 0 };
        struct timespec t = { 0, 50000000L };
        double t0 = now();
        if (ppoll(&p, 1, &t, NULL) != 0 || now() - t0 < 0.04) early++;
    }
    printf("loop: %d of 10 rounds came back early\n", early);
    fflush(stdout);
    pid_t kid = fork();
    if (kid == 0) {
        show("the fork's copy, empty pipe", ep, 300);
        fflush(stdout);
        _exit(0);
    }
    waitpid(kid, NULL, 0);
    kid = fork();
    if (kid == 0) {
        usleep(200000);
        write(pfd[1], "x", 1);
        _exit(0);
    }
    show("written by a child", ep, 4000);
    struct epoll_event got[4];
    printf("epoll_wait: %d\n", epoll_wait(ep, got, 4, 0));
    show("written, two sets deep", outer, 300);
    waitpid(kid, NULL, 0);
    read(pfd[0], &c, 1);
    show("read again", ep, 300);
    show("read again, two sets deep", outer, 300);
    return 0;
}
