/* The socket calls from several threads at once (socket, socketpair,
 * bind, listen, accept, connect, sendto, recvfrom, sendmmsg, recvmmsg, the
 * names, the options and shutdown without the kernel lock).
 *
 * - One thread listens on a named unix socket and serves connections one at
 *   a time, in a blocking accept: each sends a word, gets it back reversed,
 *   then shuts its side and sees the end.
 * - Two threads connect to it over and over, checking the options and names
 *   of what they made.
 * - One thread makes socketpairs and passes datagram-sized messages through
 *   them, two at a time with sendmmsg and recvmmsg.
 * - One thread makes and closes internet sockets and asks their kind. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define ROUNDS 200
#define CLIENTS 2

static struct sockaddr_un addr;
static socklen_t addr_len;
static pthread_barrier_t listening;
static atomic_int bad;

static void fail(const char *what, int n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%d, errno %d)\n", what, n, errno);
}

static int read_all(int fd, char *buf, int len) {
    int got = 0;
    while (got < len) {
        ssize_t r = recvfrom(fd, buf + got, len - got, 0, 0, 0);
        if (r <= 0) return -1;
        got += r;
    }
    return got;
}

static void *server(void *arg) {
    (void)arg;
    int l = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (l < 0 || bind(l, (struct sockaddr *)&addr, addr_len) || listen(l, 8)) fail("listen", l);
    pthread_barrier_wait(&listening);
    for (int i = 0; i < CLIENTS * ROUNDS; i++) {
        struct sockaddr_un from;
        socklen_t from_len = sizeof from;
        int s = accept4(l, (struct sockaddr *)&from, &from_len, SOCK_CLOEXEC);
        if (s < 0) { fail("accept4", i); continue; }
        if (from.sun_family != AF_UNIX) fail("accepted family", from.sun_family);
        char w[8], r[8];
        if (read_all(s, w, 8) != 8) { fail("server read", i); close(s); continue; }
        for (int j = 0; j < 8; j++) r[j] = w[7 - j];
        if (sendto(s, r, 8, 0, 0, 0) != 8) fail("server send", i);
        /* The client shuts its side after the reply: the end. */
        char c;
        if (recvfrom(s, &c, 1, 0, 0, 0) != 0) fail("no end after the client's shutdown", i);
        close(s);
    }
    close(l);
    return 0;
}

static void *client(void *arg) {
    int id = (int)(long)arg;
    pthread_barrier_wait(&listening);
    for (int i = 0; i < ROUNDS; i++) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        if (c < 0) { fail("socket", i); continue; }
        if (connect(c, (struct sockaddr *)&addr, addr_len)) { fail("connect", i); close(c); continue; }
        int ty = 0;
        socklen_t len = sizeof ty;
        if (getsockopt(c, SOL_SOCKET, SO_TYPE, &ty, &len) || ty != SOCK_STREAM) fail("SO_TYPE", ty);
        int err = -1;
        len = sizeof err;
        if (getsockopt(c, SOL_SOCKET, SO_ERROR, &err, &len) || err != 0) fail("SO_ERROR", err);
        struct sockaddr_un me;
        len = sizeof me;
        if (getsockname(c, (struct sockaddr *)&me, &len) || me.sun_family != AF_UNIX) fail("getsockname", i);
        int one = 1;
        if (setsockopt(c, SOL_SOCKET, SO_KEEPALIVE, &one, sizeof one)) fail("setsockopt", i);
        char w[8], r[8];
        snprintf(w, sizeof w, "%c%06d", 'a' + id, i);
        w[7] = (char)('A' + i % 26);
        if (sendto(c, w, 8, 0, 0, 0) != 8) fail("client send", i);
        if (read_all(c, r, 8) != 8) fail("client read", i);
        for (int j = 0; j < 8; j++)
            if (r[j] != w[7 - j]) { fail("reply not reversed", i); break; }
        if (shutdown(c, SHUT_WR)) fail("shutdown", i);
        close(c);
    }
    return 0;
}

static void *pairs(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        int p[2];
        if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, p)) { fail("socketpair", i); continue; }
        char a[4] = { 'p', 'q', (char)i, 0 }, b[4] = { 'r', 's', (char)i, 1 };
        struct iovec iov[2] = { { a, 4 }, { b, 4 } };
        struct mmsghdr out[2];
        memset(out, 0, sizeof out);
        out[0].msg_hdr.msg_iov = &iov[0];
        out[0].msg_hdr.msg_iovlen = 1;
        out[1].msg_hdr.msg_iov = &iov[1];
        out[1].msg_hdr.msg_iovlen = 1;
        if (sendmmsg(p[0], out, 2, 0) != 2 || out[0].msg_len != 4 || out[1].msg_len != 4) fail("sendmmsg", i);
        char got[8];
        if (read_all(p[1], got, 8) != 8 || memcmp(got, a, 4) || memcmp(got + 4, b, 4)) fail("pair bytes", i);
        /* And back with recvmmsg: one message of what is there. */
        if (sendto(p[1], "back", 4, 0, 0, 0) != 4) fail("send back", i);
        char back[8];
        struct iovec biov = { back, sizeof back };
        struct mmsghdr in;
        memset(&in, 0, sizeof in);
        in.msg_hdr.msg_iov = &biov;
        in.msg_hdr.msg_iovlen = 1;
        if (recvmmsg(p[0], &in, 1, 0, 0) != 1 || in.msg_len != 4 || memcmp(back, "back", 4)) fail("recvmmsg", i);
        close(p[0]);
        close(p[1]);
    }
    return 0;
}

static void *inet(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        int s = socket(AF_INET, i % 2 ? SOCK_DGRAM : SOCK_STREAM, 0);
        if (s < 0) { fail("inet socket", i); continue; }
        int dom = 0, ty = 0;
        socklen_t len = sizeof dom;
        if (getsockopt(s, SOL_SOCKET, SO_DOMAIN, &dom, &len) || dom != AF_INET) fail("SO_DOMAIN", dom);
        len = sizeof ty;
        if (getsockopt(s, SOL_SOCKET, SO_TYPE, &ty, &len) || ty != (i % 2 ? SOCK_DGRAM : SOCK_STREAM)) fail("inet SO_TYPE", ty);
        struct sockaddr sa;
        len = sizeof sa;
        errno = 0;
        if (getpeername(s, &sa, &len) == 0 || errno != ENOTCONN) fail("getpeername not ENOTCONN", errno);
        close(s);
        errno = 0;
        if (socket(AF_INET6 + 100, SOCK_STREAM, 0) != -1 || errno != EAFNOSUPPORT) fail("bad family", errno);
    }
    return 0;
}

int main(void) {
    addr.sun_family = AF_UNIX;
    /* Its own directory: concurrent runs under gexc share /tmp, and the
     * guest's pid is the same in each. */
    static char dir[64] = "/tmp/sock-threads-XXXXXX";
    if (!mkdtemp(dir)) return 2;
    snprintf(addr.sun_path, sizeof addr.sun_path, "%s/s", dir);
    unlink(addr.sun_path);
    addr_len = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + strlen(addr.sun_path) + 1);
    pthread_barrier_init(&listening, 0, 1 + CLIENTS);
    pthread_t t[5];
    pthread_create(&t[0], 0, server, 0);
    for (int i = 0; i < CLIENTS; i++) pthread_create(&t[1 + i], 0, client, (void *)(long)i);
    pthread_create(&t[3], 0, pairs, 0);
    pthread_create(&t[4], 0, inet, 0);
    for (int i = 0; i < 5; i++) pthread_join(t[i], 0);
    unlink(addr.sun_path);
    rmdir(dir);
    printf("rounds %d bad %d\n", ROUNDS, atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
