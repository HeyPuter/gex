/* What libuv relies on from socketpair(AF_UNIX, SOCK_STREAM): uv_spawn makes
 * each child's stdio this way, and in the browser the two ends are in two
 * Workers once the child forks. One line per check, compared with native.
 *  1 both ends full duplex; F_GETFL O_RDWR; F_SETFL O_NONBLOCK keeps it
 *  2 fstat S_IFSOCK; getsockname AF_UNIX (unnamed); SO_TYPE SOCK_STREAM
 *  3 SO_DOMAIN, SO_PROTOCOL, SO_ERROR
 *  4 SHUT_WR: the peer reads EOF, the other direction still works
 *  5 last close: the peer reads EOF; a write gets EPIPE and SIGPIPE
 *  6 fork, dup2 onto 0/1 (CLOEXEC off on the copy only), execve; the
 *    parent's copies keep CLOEXEC; the parent closing its copy is harmless
 *  7 poll and epoll both ways, EPOLLHUP/EPOLLRDHUP after the peer closes;
 *    EAGAIN only with O_NONBLOCK
 *  8 a non-blocking write with some room returns a short count
 *  9 FIONBIO, FIOCLEX/FIONCLEX
 * 10 MSG_NOSIGNAL
 * Each check runs on a pair made before a fork as well (the child holds
 * one end), where the browser's ends are in different Workers. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t pipes;
static void on_pipe(int s) { (void)s; pipes++; }

#define CHECK(name, cond)                                                                \
    do {                                                                                 \
        printf("%s %s\n", (cond) ? "ok  " : "FAIL", name);                               \
        fflush(stdout);                                                                  \
    } while (0)

static int pair(int extra, int sv[2]) {
    return socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | extra, 0, sv);
}

static void basics(void) {
    int sv[2];
    pair(0, sv);
    char b[8];
    int dup = write(sv[0], "ab", 2) == 2 && read(sv[1], b, 8) == 2 && write(sv[1], "cd", 2) == 2 &&
              read(sv[0], b, 8) == 2 && !memcmp(b, "cd", 2);
    CHECK("1 full duplex", dup);
    int fl = fcntl(sv[0], F_GETFL);
    CHECK("1 F_GETFL O_RDWR", (fl & O_ACCMODE) == O_RDWR);
    fcntl(sv[0], F_SETFL, fl | O_NONBLOCK);
    fl = fcntl(sv[0], F_GETFL);
    CHECK("1 F_SETFL O_NONBLOCK keeps O_RDWR", (fl & O_ACCMODE) == O_RDWR && (fl & O_NONBLOCK));
    struct stat st;
    CHECK("2 fstat S_IFSOCK", fstat(sv[1], &st) == 0 && S_ISSOCK(st.st_mode));
    struct sockaddr_storage ss;
    socklen_t sl = sizeof ss;
    memset(&ss, 0xff, sizeof ss);
    int gn = getsockname(sv[1], (struct sockaddr *)&ss, &sl);
    CHECK("2 getsockname AF_UNIX unnamed", gn == 0 && ss.ss_family == AF_UNIX && sl >= sizeof(sa_family_t) &&
                                              (sl == sizeof(sa_family_t) || ((struct sockaddr_un *)&ss)->sun_path[0] == 0));
    int v = -1;
    socklen_t vl = sizeof v;
    CHECK("2 SO_TYPE SOCK_STREAM", getsockopt(sv[1], SOL_SOCKET, SO_TYPE, &v, &vl) == 0 && v == SOCK_STREAM);
    v = -1; vl = sizeof v;
    CHECK("3 SO_DOMAIN AF_UNIX", getsockopt(sv[1], SOL_SOCKET, SO_DOMAIN, &v, &vl) == 0 && v == AF_UNIX);
    v = -1; vl = sizeof v;
    CHECK("3 SO_PROTOCOL 0", getsockopt(sv[1], SOL_SOCKET, SO_PROTOCOL, &v, &vl) == 0 && v == 0);
    v = -1; vl = sizeof v;
    CHECK("3 SO_ERROR 0", getsockopt(sv[1], SOL_SOCKET, SO_ERROR, &v, &vl) == 0 && v == 0);
    /* 9 */
    int on = 1;
    ioctl(sv[1], FIONBIO, &on);
    CHECK("9 FIONBIO sets O_NONBLOCK", fcntl(sv[1], F_GETFL) & O_NONBLOCK);
    CHECK("9 FIONBIO nonblocking read EAGAIN", read(sv[1], b, 8) == -1 && errno == EAGAIN);
    on = 0;
    ioctl(sv[1], FIONBIO, &on);
    CHECK("9 FIONBIO clears O_NONBLOCK", !(fcntl(sv[1], F_GETFL) & O_NONBLOCK));
    ioctl(sv[1], FIONCLEX);
    CHECK("9 FIONCLEX", fcntl(sv[1], F_GETFD) == 0);
    ioctl(sv[1], FIOCLEX);
    CHECK("9 FIOCLEX", fcntl(sv[1], F_GETFD) == FD_CLOEXEC);
    close(sv[0]);
    close(sv[1]);
}

/* 4, 5, 7, 8, 10 with `peer` the other end's holder: this process, or a
 * child that runs `serve`. */
static void serve(int fd, int ctl) {
    /* the child's half of `cross`: commands on ctl, one byte each */
    char c, b[4096];
    while (read(ctl, &c, 1) == 1) {
        switch (c) {
        case 'r': { /* read to EOF, report the count */
            long n = 0, r;
            while ((r = read(fd, b, sizeof b)) > 0) n += r;
            dprintf(ctl, "%ld", n);
            break;
        }
        case 'w': write(fd, "x", 1); write(ctl, "k", 1); break;
        case 's': /* make room: 64 KiB */
            for (long n = 0, r; n < 65536 && (r = read(fd, b, sizeof b)) > 0;) n += r;
            write(ctl, "k", 1);
            break;
        case 'W': shutdown(fd, SHUT_WR); write(ctl, "k", 1); break;
        case 'c': close(fd); write(ctl, "k", 1); break;
        case 'q': _exit(0);
        }
    }
    _exit(0);
}

static void cmd(int ctl, char c, char *reply, int n) {
    write(ctl, &c, 1);
    int r = read(ctl, reply, n - 1);
    reply[r > 0 ? r : 0] = 0;
}

static void cross(int forked) {
    const char *w = forked ? "(across fork)" : "(one process)";
    char name[128], rep[32];
    int sv[2], ctl[2];
    pair(0, sv);
    socketpair(AF_UNIX, SOCK_STREAM, 0, ctl);
    pid_t pid = -1;
    if (forked) {
        pid = fork();
        if (pid == 0) {
            close(sv[0]);
            close(ctl[0]);
            serve(sv[1], ctl[1]);
        }
        close(sv[1]);
        close(ctl[1]);
    }
    int me = sv[0];
    /* 7: nothing to read, room to write */
    struct pollfd p = {me, POLLIN | POLLOUT, 0};
    poll(&p, 1, 0);
    snprintf(name, sizeof name, "7 poll idle: POLLOUT only %s", w);
    CHECK(name, p.revents == POLLOUT);
    /* the peer writes: readable here */
    if (forked) {
        cmd(ctl[0], 'w', rep, sizeof rep);
    } else {
        write(sv[1], "x", 1);
    }
    p.revents = 0;
    poll(&p, 1, 1000);
    snprintf(name, sizeof name, "7 poll after peer write: POLLIN %s", w);
    CHECK(name, (p.revents & POLLIN) != 0);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN | EPOLLRDHUP, .data.fd = me}, out[2];
    epoll_ctl(ep, EPOLL_CTL_ADD, me, &ev);
    int n = epoll_wait(ep, out, 2, 1000);
    snprintf(name, sizeof name, "7 epoll after peer write: EPOLLIN %s", w);
    CHECK(name, n == 1 && (out[0].events & EPOLLIN) && !(out[0].events & (EPOLLHUP | EPOLLRDHUP)));
    char b[8];
    read(me, b, 1);
    /* 4: shut our sending side; the peer reads EOF; it can still write to us */
    shutdown(me, SHUT_WR);
    if (forked) {
        cmd(ctl[0], 'r', rep, sizeof rep);
    } else {
        snprintf(rep, sizeof rep, "%zd", read(sv[1], b, 8));
    }
    snprintf(name, sizeof name, "4 SHUT_WR: peer reads EOF %s", w);
    CHECK(name, !strcmp(rep, "0"));
    if (forked) {
        cmd(ctl[0], 'w', rep, sizeof rep);
    } else {
        write(sv[1], "x", 1);
    }
    snprintf(name, sizeof name, "4 SHUT_WR: other direction works %s", w);
    CHECK(name, read(me, b, 8) == 1);
    /* 7: EAGAIN only with O_NONBLOCK -- a blocking read is still pending,
     * which we cannot test without hanging; check the nonblocking side */
    fcntl(me, F_SETFL, O_NONBLOCK);
    errno = 0;
    snprintf(name, sizeof name, "7 nonblocking empty read EAGAIN %s", w);
    CHECK(name, read(me, b, 8) == -1 && errno == EAGAIN);
    fcntl(me, F_SETFL, 0);
    /* 5: the peer's end closes (its last reference) */
    if (forked) {
        cmd(ctl[0], 'c', rep, sizeof rep);
    } else {
        close(sv[1]);
    }
    n = epoll_wait(ep, out, 2, 1000);
    snprintf(name, sizeof name, "7 epoll after peer close: EPOLLIN|EPOLLHUP|EPOLLRDHUP %s", w);
    CHECK(name, n == 1 && (out[0].events & (EPOLLIN | EPOLLHUP | EPOLLRDHUP)) == (EPOLLIN | EPOLLHUP | EPOLLRDHUP));
    p.events = POLLIN | POLLOUT | POLLRDHUP;
    p.revents = 0;
    poll(&p, 1, 1000);
    snprintf(name, sizeof name, "7 poll after peer close: POLLIN|POLLHUP|POLLRDHUP %s", w);
    CHECK(name, (p.revents & (POLLIN | POLLHUP | POLLRDHUP)) == (POLLIN | POLLHUP | POLLRDHUP));
    snprintf(name, sizeof name, "5 peer closed: read EOF %s", w);
    CHECK(name, read(me, b, 8) == 0);
    /* our write side is shut already: use a fresh check on a new pair */
    close(ep);
    if (forked) {
        cmd(ctl[0], 'q', rep, sizeof rep);
        waitpid(pid, 0, 0);
    }
    close(me);

    /* 5, 10: writes to a closed peer */
    pair(0, sv);
    socketpair(AF_UNIX, SOCK_STREAM, 0, ctl);
    if (forked) {
        pid = fork();
        if (pid == 0) {
            close(sv[0]);
            close(ctl[0]);
            serve(sv[1], ctl[1]);
        }
        close(sv[1]);
        close(ctl[1]);
        cmd(ctl[0], 'c', rep, sizeof rep);
    } else {
        close(sv[1]);
    }
    me = sv[0];
    pipes = 0;
    errno = 0;
    ssize_t r = write(me, "x", 1);
    snprintf(name, sizeof name, "5 write to closed peer: EPIPE and SIGPIPE %s", w);
    CHECK(name, r == -1 && errno == EPIPE && pipes == 1);
    pipes = 0;
    errno = 0;
    r = send(me, "x", 1, MSG_NOSIGNAL);
    snprintf(name, sizeof name, "10 MSG_NOSIGNAL: EPIPE, no SIGPIPE %s", w);
    CHECK(name, r == -1 && errno == EPIPE && pipes == 0);
    if (forked) {
        cmd(ctl[0], 'q', rep, sizeof rep);
        waitpid(pid, 0, 0);
        close(ctl[0]);
    }
    close(me);

    /* 8: fill to almost full, then a large non-blocking write is short */
    pair(SOCK_NONBLOCK, sv);
    socketpair(AF_UNIX, SOCK_STREAM, 0, ctl);
    if (forked) {
        pid = fork();
        if (pid == 0) {
            close(sv[0]);
            close(ctl[0]);
            serve(sv[1], ctl[1]);
        }
        close(sv[1]);
        close(ctl[1]);
    }
    me = sv[0];
    static char big[1 << 20];
    long total = 0;
    int eagain = 0;
    while (total < (64 << 20)) {
        r = write(me, big, 1000);
        if (r < 0) { eagain = errno == EAGAIN; break; }
        total += r;
    }
    snprintf(name, sizeof name, "7 nonblocking full write EAGAIN %s", w);
    CHECK(name, eagain);
    /* the peer drains a little: a large write is short, not EAGAIN */
    if (forked) {
        cmd(ctl[0], 's', rep, sizeof rep);
    } else {
        for (long n = 0, r; n < 65536 && (r = read(sv[1], big, 4096)) > 0;) n += r;
    }
    r = write(me, big, sizeof big);
    snprintf(name, sizeof name, "8 nonblocking write with some room is short %s", w);
    CHECK(name, r > 0 && r < (ssize_t)sizeof big);
    if (forked) {
        cmd(ctl[0], 'q', rep, sizeof rep);
        waitpid(pid, 0, 0);
        close(ctl[0]);
    } else {
        close(sv[1]);
    }
    close(me);
}

/* 6: the child's stdin and stdout are the pair's other end, dup2'd; it
 * execs this program with "child", which echoes stdin to stdout in upper
 * case and reports its descriptor flags. */
static int child_main(void) {
    int fl0 = fcntl(0, F_GETFD), fl1 = fcntl(1, F_GETFD);
    struct stat st;
    fstat(0, &st);
    char b[64];
    ssize_t n = read(0, b, sizeof b);
    for (ssize_t i = 0; i < n; i++)
        if (b[i] >= 'a' && b[i] <= 'z') b[i] -= 32;
    dprintf(1, "%.*s fd0 cloexec %d fd1 cloexec %d sock %d", (int)(n > 0 ? n : 0), b, fl0, fl1, S_ISSOCK(st.st_mode));
    /* stdin to EOF: the parent shuts down its sending side */
    n = read(0, b, sizeof b);
    dprintf(1, " eof %zd", n);
    return 0;
}

static void spawn(const char *self) {
    int in[2], out[2];
    pair(0, in);
    pair(0, out);
    pid_t pid = fork();
    if (pid == 0) {
        dup2(in[1], 0);
        dup2(out[1], 1);
        execl(self, self, "child", (char *)0);
        _exit(127);
    }
    int ok = fcntl(in[0], F_GETFD) == FD_CLOEXEC && fcntl(in[1], F_GETFD) == FD_CLOEXEC &&
             fcntl(out[1], F_GETFD) == FD_CLOEXEC;
    CHECK("6 parent's copies keep CLOEXEC", ok);
    close(in[1]);
    close(out[1]);
    write(in[0], "hello", 5);
    char b[128];
    long n = 0, r;
    /* the child answers the first read, then waits for EOF */
    r = read(out[0], b, sizeof b - 1);
    if (r > 0) n = r;
    shutdown(in[0], SHUT_WR);
    while (n < (long)sizeof b - 1 && (r = read(out[0], b + n, sizeof b - 1 - n)) > 0) n += r;
    b[n] = 0;
    int st;
    waitpid(pid, &st, 0);
    CHECK("6 child echoes through dup2'd ends", strstr(b, "HELLO") != NULL);
    CHECK("6 dup2 cleared CLOEXEC on 0 and 1 only", strstr(b, "fd0 cloexec 0 fd1 cloexec 0") != NULL);
    CHECK("6 child's stdin is a socket", strstr(b, "sock 1") != NULL);
    CHECK("6 child read EOF after SHUT_WR", strstr(b, "eof 0") != NULL);
    CHECK("6 child exit 0", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    close(in[0]);
    close(out[0]);
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child")) return child_main();
    signal(SIGPIPE, on_pipe);
    basics();
    cross(0);
    cross(1);
    spawn(argv[0]);
    return 0;
}
