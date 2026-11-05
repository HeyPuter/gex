/* The signal mask of epoll_pwait, ppoll and pselect6: the thread's mask for
 * the call's duration, the old one back afterwards (Linux's
 * set_user_sigmask / restore_saved_sigmask_unless). foot blocks SIGTERM
 * and takes it only through epoll_pwait's mask; gex ignored the mask, so
 * foot never heard a SIGTERM.
 *
 * For each call, with SIGTERM blocked:
 *   waits:   another process sends SIGTERM while the call waits (on a pipe
 *            nobody writes, 5 s bound) with a mask that lets it through:
 *            EINTR, the handler ran, SIGTERM blocked again afterwards;
 *   pending: SIGTERM already pending when the call starts: EINTR at once,
 *            the handler ran;
 *   ready:   SIGTERM pending and a descriptor ready: the call answers 1,
 *            the handler does not run inside it (the old mask is back
 *            before any signal is delivered), and runs once SIGTERM is
 *            unblocked.
 * The output is the same natively. */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t got;
static void on_term(int s) { (void)s; got++; }

static int p[2];

/* One call: kind 0 epoll_pwait, 1 ppoll, 2 pselect6, on p[0], with `mask`. */
static int wait_with(int kind, const sigset_t *mask, int ms) {
    if (kind == 0) {
        int ep = epoll_create1(0);
        struct epoll_event ev = {.events = EPOLLIN, .data.fd = p[0]}, out;
        epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev);
        int r = epoll_pwait(ep, &out, 1, ms, mask);
        int e = errno;
        close(ep);
        errno = e;
        return r;
    }
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    if (kind == 1) {
        struct pollfd f = {.fd = p[0], .events = POLLIN};
        return ppoll(&f, 1, &ts, mask);
    }
    fd_set r;
    FD_ZERO(&r);
    FD_SET(p[0], &r);
    return pselect(p[0] + 1, &r, 0, 0, &ts, mask);
}

static int term_blocked(void) {
    sigset_t now;
    sigprocmask(SIG_BLOCK, 0, &now);
    return sigismember(&now, SIGTERM);
}

int main(void) {
    static const char *names[] = {"epoll_pwait", "ppoll", "pselect6"};
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_term;
    sigaction(SIGTERM, &sa, 0);
    sigset_t term, open_;
    sigemptyset(&term);
    sigaddset(&term, SIGTERM);
    sigemptyset(&open_);
    sigprocmask(SIG_BLOCK, &term, 0);
    if (pipe(p)) return 1;
    int fail = 0;
    for (int kind = 0; kind < 3; kind++) {
        /* waits */
        got = 0;
        pid_t parent = getpid(), c = fork();
        if (c == 0) {
            struct timespec d = {0, 100000000L};
            nanosleep(&d, 0);
            kill(parent, SIGTERM);
            _exit(0);
        }
        int r = wait_with(kind, &open_, 5000);
        int e = errno;
        waitpid(c, 0, 0);
        int ok = r == -1 && e == EINTR && got == 1 && term_blocked();
        printf("%s waits: %s (r=%d %s, handler %d, blocked after %d)\n", names[kind], ok ? "ok" : "FAIL", r,
               r < 0 ? strerror(e) : "-", (int)got, term_blocked());
        fail |= !ok;

        /* pending */
        got = 0;
        kill(getpid(), SIGTERM);
        r = wait_with(kind, &open_, 5000);
        e = errno;
        ok = r == -1 && e == EINTR && got == 1 && term_blocked();
        printf("%s pending: %s (r=%d %s, handler %d, blocked after %d)\n", names[kind], ok ? "ok" : "FAIL", r,
               r < 0 ? strerror(e) : "-", (int)got, term_blocked());
        fail |= !ok;

        /* ready */
        got = 0;
        if (write(p[1], "x", 1) != 1) return 1;
        kill(getpid(), SIGTERM);
        r = wait_with(kind, &open_, 5000);
        int inside = got;
        char ch;
        if (read(p[0], &ch, 1) != 1) return 1;
        sigprocmask(SIG_UNBLOCK, &term, 0);
        int after = got;
        sigprocmask(SIG_BLOCK, &term, 0);
        ok = r == 1 && inside == 0 && after == 1;
        printf("%s ready: %s (r=%d, handler %d inside, %d once unblocked)\n", names[kind], ok ? "ok" : "FAIL", r,
               inside, after);
        fail |= !ok;
    }
    return fail;
}
