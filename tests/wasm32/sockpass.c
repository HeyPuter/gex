/* A connected TCP socket handed to another process, the way Wine's server
 * hands its clients every socket (it makes and connects them, then passes the
 * descriptor over SCM_RIGHTS): the connection must outlive whichever holder
 * closes first, and shutdown is the socket's, whichever descriptor it is
 * called on. In a page every process has its own memory, so this is the
 * descriptor carried as a name and rebuilt on the same connection
 * (shims/browser carry_name, "inet:").
 *
 *   sockpass               loopback: a listener in this process
 *   sockpass HOST PORT     and HTTP/1.0 GETs of HOST:PORT (a page's Wisp)
 *
 * Loopback (each prints ok/FAIL):
 *   scm: the connected end passed to a child over SCM_RIGHTS, the parent
 *        closing its copy at once; the child talks over it
 *   fork-child-closes: inherited across fork, the child closes its copy and
 *        exits; the parent talks over it after
 *   shutdown: inherited across fork, the child calls shutdown(SHUT_WR) while
 *        the parent still holds the descriptor; the far end reads EOF
 * Remote (HOST PORT): the first two again, each a GET answered with HTTP.
 * Every reader first peeks a byte (recv MSG_PEEK) and must find it still
 * there for the read after. */
#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
static void ok(int c, const char *what) { printf("%s %s\n", c ? "ok" : "FAIL", what); if (!c) fails++; }

static void send_fd(int u, int fd)
{
    char b = 'f', ctl[CMSG_SPACE(sizeof(int))];
    struct iovec v = { &b, 1 };
    struct msghdr m = { 0 };
    struct cmsghdr *c;
    memset(ctl, 0, sizeof ctl);
    m.msg_iov = &v; m.msg_iovlen = 1; m.msg_control = ctl; m.msg_controllen = sizeof ctl;
    c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof(int));
    if (sendmsg(u, &m, 0) != 1) perror("sendmsg");
}

static int recv_fd(int u)
{
    char b, ctl[CMSG_SPACE(sizeof(int))];
    struct iovec v = { &b, 1 };
    struct msghdr m = { 0 };
    struct cmsghdr *c;
    int fd = -1;
    m.msg_iov = &v; m.msg_iovlen = 1; m.msg_control = ctl; m.msg_controllen = sizeof ctl;
    if (recvmsg(u, &m, 0) != 1) return -1;
    for (c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c))
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) memcpy(&fd, CMSG_DATA(c), sizeof(int));
    return fd;
}

/* Bytes before EOF, or -1; waits at most 20 s for each. */
static long drain(int s, char *first, size_t cap)
{
    char buf[4096];
    long total = 0;
    for (;;) {
        struct pollfd p = { s, POLLIN, 0 };
        ssize_t n;
        if (poll(&p, 1, 20000) != 1) return -1;
        n = read(s, buf, sizeof buf);
        if (n < 0) return -1;
        if (n == 0) return total;
        if (first && total == 0) { size_t k = (size_t)n < cap - 1 ? (size_t)n : cap - 1; memcpy(first, buf, k); first[k] = 0; }
        total += n;
    }
}

/* recv(MSG_PEEK) of one byte, once readable: it is WANT and still queued
 * (the next read starts with it). Wine's wineserver peeks every readable
 * stream socket this way, to tell data from the far end having closed. */
static int peeked(int s, char want)
{
    char c = 0;
    struct pollfd p = { s, POLLIN, 0 };
    return poll(&p, 1, 20000) == 1 && recv(s, &c, 1, MSG_PEEK) == 1 && c == want;
}

/* An HTTP/1.0 GET over s: 1 if an HTTP status line and a body came back. */
static int get(int s, const char *host)
{
    char req[256], first[64] = { 0 };
    long n;
    snprintf(req, sizeof req, "GET / HTTP/1.0\r\nHost: %s\r\nConnection: close\r\n\r\n", host);
    if (write(s, req, strlen(req)) != (ssize_t)strlen(req)) return 0;
    if (!peeked(s, 'H')) return 0;
    n = drain(s, first, sizeof first);
    return n > 12 && !strncmp(first, "HTTP/1.", 7);
}

static int dial(const char *host, const char *port)
{
    struct addrinfo hints = { 0 }, *ai;
    int s;
    hints.ai_family = AF_INET; hints.ai_socktype = SOCK_STREAM;
    if (getaddrinfo(host, port, &hints, &ai)) return -1;
    s = socket(AF_INET, SOCK_STREAM, 0);
    if (s < 0 || connect(s, ai->ai_addr, ai->ai_addrlen)) { freeaddrinfo(ai); return -1; }
    freeaddrinfo(ai);
    return s;
}

static int status_of(pid_t pid)
{
    int st = 0;
    if (waitpid(pid, &st, 0) != pid) return -1;
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128;
}

/* A loopback pair: *c connected to *a, the listener closed. */
static int pair(int *c, int *a)
{
    struct sockaddr_in in = { 0 };
    socklen_t len = sizeof in;
    int l = socket(AF_INET, SOCK_STREAM, 0);
    in.sin_family = AF_INET; in.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (l < 0 || bind(l, (struct sockaddr *)&in, sizeof in) || getsockname(l, (struct sockaddr *)&in, &len) || listen(l, 4)) return 0;
    *c = socket(AF_INET, SOCK_STREAM, 0);
    if (*c < 0 || connect(*c, (struct sockaddr *)&in, sizeof in)) return 0;
    *a = accept(l, 0, 0);
    close(l);
    return *a >= 0;
}

/* The child's side of a loopback exchange: "ping" out, "pong" back. */
static int ping(int s)
{
    char b[8] = { 0 };
    struct pollfd p = { s, POLLIN, 0 };
    return write(s, "ping", 4) == 4 && poll(&p, 1, 20000) == 1 && read(s, b, sizeof b) == 4 && !memcmp(b, "pong", 4);
}

static int pong(int s)
{
    char b[8] = { 0 };
    struct pollfd p = { s, POLLIN, 0 };
    return poll(&p, 1, 20000) == 1 && peeked(s, 'p') && read(s, b, sizeof b) == 4 && !memcmp(b, "ping", 4) && write(s, "pong", 4) == 4;
}

/* Passed over SCM_RIGHTS, the sender closing its copy at once. TALK runs in
 * the child on the passed descriptor; PEER in the parent, if any. */
static void scm(const char *what, int s, int (*talk)(int, const char *), const char *host, int peer)
{
    int u[2];
    pid_t pid;
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, u)) { ok(0, what); return; }
    pid = fork();
    if (pid == 0) {
        int fd;
        close(u[0]);
        if (peer >= 0) close(peer);
        close(s);
        fd = recv_fd(u[1]);
        _exit(fd >= 0 && talk(fd, host) ? 0 : 1);
    }
    close(u[1]);
    send_fd(u[0], s);
    close(s);
    close(u[0]);
    if (peer >= 0) { int r = pong(peer); close(peer); if (!r) { status_of(pid); ok(0, what); return; } }
    ok(status_of(pid) == 0, what);
}

/* Inherited across fork; the child closes its copy and exits; then TALK in
 * the parent. */
static void child_closes(const char *what, int s, int (*talk)(int, const char *), const char *host, int peer)
{
    pid_t pid = fork();
    if (pid == 0) { close(s); if (peer >= 0) close(peer); _exit(0); }
    if (status_of(pid) != 0) { ok(0, what); return; }
    if (peer >= 0) {
        pid_t p2 = fork();
        if (p2 == 0) { close(s); _exit(pong(peer) ? 0 : 1); }
        close(peer);
        ok(talk(s, host) && status_of(p2) == 0, what);
    } else {
        ok(talk(s, host), what);
    }
    close(s);
}

static int ping_talk(int s, const char *host) { (void)host; return ping(s); }

int main(int argc, char **argv)
{
    int c, a;
    setvbuf(stdout, 0, _IONBF, 0);
    if (pair(&c, &a)) scm("loopback: passed over SCM_RIGHTS, the sender closing first", c, ping_talk, 0, a);
    else ok(0, "loopback pair (scm)");
    if (pair(&c, &a)) child_closes("loopback: inherited, the child closing first", c, ping_talk, 0, a);
    else ok(0, "loopback pair (fork)");
    if (pair(&c, &a)) {
        pid_t pid = fork();
        if (pid == 0) { close(a); _exit(shutdown(c, SHUT_WR) == 0 ? 0 : 1); }
        /* the parent keeps c open: the far end still reads EOF */
        ok(status_of(pid) == 0 && drain(a, 0, 0) == 0, "loopback: shutdown(SHUT_WR) in the child, the parent still holding it");
        close(c); close(a);
    } else ok(0, "loopback pair (shutdown)");
    if (argc > 2) {
        int s = dial(argv[1], argv[2]);
        if (s >= 0) scm("remote: passed over SCM_RIGHTS, the sender closing first", s, get, argv[1], -1);
        else ok(0, "remote: connect (scm)");
        s = dial(argv[1], argv[2]);
        if (s >= 0) child_closes("remote: inherited, the child closing first", s, get, argv[1], -1);
        else ok(0, "remote: connect (fork)");
    }
    printf("sockpass: %d failed\n", fails);
    return fails != 0;
}
