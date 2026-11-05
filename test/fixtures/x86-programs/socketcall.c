// The socket calls through i386's socketcall(2), as a glibc built for
// kernels before 4.3 makes every one of them: a socketpair, send and recv,
// sendto and recvfrom, getsockopt, shutdown, and a listening socket's
// bind, getsockname, connect and accept. On x86-64 (no socketcall) the
// same calls are made directly, so both builds print the same lines.
#define _GNU_SOURCE
#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <unistd.h>

#ifdef __i386__
static long sc(int call, unsigned long a0, unsigned long a1, unsigned long a2, unsigned long a3,
               unsigned long a4, unsigned long a5) {
    unsigned long args[6] = {a0, a1, a2, a3, a4, a5};
    return syscall(SYS_socketcall, call, args);
}
#define SOCKET(d, t, p) sc(1, d, t, p, 0, 0, 0)
#define BIND(f, a, l) sc(2, f, (unsigned long)(a), l, 0, 0, 0)
#define CONNECT(f, a, l) sc(3, f, (unsigned long)(a), l, 0, 0, 0)
#define LISTEN(f, b) sc(4, f, b, 0, 0, 0, 0)
#define ACCEPT(f, a, l) sc(5, f, (unsigned long)(a), (unsigned long)(l), 0, 0, 0)
#define GETSOCKNAME(f, a, l) sc(6, f, (unsigned long)(a), (unsigned long)(l), 0, 0, 0)
#define SOCKETPAIR(d, t, p, v) sc(8, d, t, p, (unsigned long)(v), 0, 0)
#define SEND(f, b, n, fl) sc(9, f, (unsigned long)(b), n, fl, 0, 0)
#define RECV(f, b, n, fl) sc(10, f, (unsigned long)(b), n, fl, 0, 0)
#define SENDTO(f, b, n, fl, a, l) sc(11, f, (unsigned long)(b), n, fl, (unsigned long)(a), l)
#define RECVFROM(f, b, n, fl, a, l) sc(12, f, (unsigned long)(b), n, fl, (unsigned long)(a), (unsigned long)(l))
#define SHUTDOWN(f, h) sc(13, f, h, 0, 0, 0, 0)
#define GETSOCKOPT(f, lv, o, v, l) sc(15, f, lv, o, (unsigned long)(v), (unsigned long)(l), 0)
#else
#define SOCKET socket
#define BIND bind
#define CONNECT connect
#define LISTEN listen
#define ACCEPT accept
#define GETSOCKNAME getsockname
#define SOCKETPAIR socketpair
#define SEND send
#define RECV recv
#define SENDTO sendto
#define RECVFROM recvfrom
#define SHUTDOWN shutdown
#define GETSOCKOPT getsockopt
#endif

int main(void) {
    int sv[2];
    char buf[32];
    if (SOCKETPAIR(AF_UNIX, SOCK_STREAM, 0, sv) != 0) { printf("socketpair: %s\n", strerror(errno)); return 1; }
    printf("socketpair ok\n");
    printf("send %ld\n", (long)SEND(sv[0], "hello", 5, 0));
    memset(buf, 0, sizeof buf);
    printf("recv %ld %s\n", (long)RECV(sv[1], buf, sizeof buf, 0), buf);
    printf("sendto %ld\n", (long)SENDTO(sv[1], "world!", 6, 0, NULL, 0));
    memset(buf, 0, sizeof buf);
    long r = RECVFROM(sv[0], buf, sizeof buf, 0, NULL, NULL);
    printf("recvfrom %ld %s\n", r, buf);
    int type = 0;
    socklen_t tl = sizeof type;
    printf("getsockopt %ld type %d\n", (long)GETSOCKOPT(sv[0], SOL_SOCKET, SO_TYPE, &type, &tl), type);
    printf("shutdown %ld\n", (long)SHUTDOWN(sv[0], SHUT_WR));
    printf("recv after shutdown %ld\n", (long)RECV(sv[1], buf, sizeof buf, 0));
    close(sv[0]);
    close(sv[1]);

    // A listening socket on an abstract name, connected to and accepted.
    int l = SOCKET(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    memcpy(a.sun_path, "\0gex-socketcall", 15);
    socklen_t al = offsetof(struct sockaddr_un, sun_path) + 15;
    printf("bind %ld\n", (long)BIND(l, (void *)&a, al));
    printf("listen %ld\n", (long)LISTEN(l, 1));
    struct sockaddr_un got;
    socklen_t gl = sizeof got;
    printf("getsockname %ld family %d\n", (long)GETSOCKNAME(l, (void *)&got, &gl), got.sun_family);
    int c = SOCKET(AF_UNIX, SOCK_STREAM, 0);
    printf("connect %ld\n", (long)CONNECT(c, (void *)&a, al));
    int s = ACCEPT(l, NULL, NULL);
    printf("accept %s\n", s >= 0 ? "ok" : strerror(errno));
    printf("send %ld\n", (long)SEND(c, "abc", 3, 0));
    memset(buf, 0, sizeof buf);
    printf("recv %ld %s\n", (long)RECV(s, buf, sizeof buf, 0), buf);
    close(s);
    close(c);
    close(l);
    return 0;
}
