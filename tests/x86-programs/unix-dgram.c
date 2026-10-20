/* AF_UNIX SOCK_DGRAM, which gex refused (EPROTONOSUPPORT): musl's
 * if_nametoindex opens one to ask an interface ioctl, syslog sends through
 * one. Message boundaries over a socketpair and truncation, MSG_TRUNC, names
 * (abstract and a path) on both ends of sendto/recvfrom and recvmsg, connect
 * and getpeername, the errors for a missing name, a stream socket's name and
 * a closed peer, non-blocking reads, poll, and a forked child sending to its
 * parent's name. */
#include <errno.h>
#include <net/if.h>
#include <poll.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int fail;
#define CHECK(c, ...) do { if (!(c)) { fail = 1; printf("FAIL "); } else printf("ok   "); printf(__VA_ARGS__); printf("\n"); } while (0)

static socklen_t addr(struct sockaddr_un *a, const char *name, int abstract)
{
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    if (abstract) {
        strcpy(a->sun_path + 1, name);
        return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(name);
    }
    strcpy(a->sun_path, name);
    return sizeof *a;
}

int main(void)
{
    char buf[64];
    CHECK(if_nametoindex("lo") > 0, "if_nametoindex(lo) is an index");

    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_DGRAM, 0, sv) == 0, "socketpair(AF_UNIX, SOCK_DGRAM)");
    send(sv[0], "a", 1, 0);
    send(sv[0], "bc", 2, 0);
    send(sv[0], "xyz", 3, 0);
    send(sv[0], "last", 4, 0);
    CHECK(recv(sv[1], buf, sizeof buf, 0) == 1, "first datagram is 1 byte");
    CHECK(recv(sv[1], buf, sizeof buf, 0) == 2 && !memcmp(buf, "bc", 2), "second is 2 bytes");
    CHECK(recv(sv[1], buf, 1, MSG_PEEK | MSG_TRUNC) == 3, "MSG_PEEK|MSG_TRUNC gives the whole length");
    CHECK(recv(sv[1], buf, 1, 0) == 1 && buf[0] == 'x', "a short read takes one byte");
    CHECK(recv(sv[1], buf, sizeof buf, 0) == 4 && !memcmp(buf, "last", 4), "and the rest of that datagram is gone");
    CHECK(recv(sv[1], buf, sizeof buf, MSG_DONTWAIT) == -1 && errno == EAGAIN, "empty: EAGAIN");
    struct pollfd p = { sv[1], POLLIN, 0 };
    CHECK(poll(&p, 1, 0) == 0, "empty: not readable");
    send(sv[0], "p", 1, 0);
    CHECK(poll(&p, 1, 0) == 1 && (p.revents & POLLIN), "a datagram: readable");
    recv(sv[1], buf, sizeof buf, 0);
    close(sv[0]);
    CHECK(send(sv[1], "x", 1, MSG_NOSIGNAL) == -1, "send to a closed peer fails (%s)", strerror(errno));

    char name[64];
    snprintf(name, sizeof name, "gex-dgram-%d", (int)getpid());
    struct sockaddr_un sa, from;
    socklen_t salen = addr(&sa, name, 1), flen;
    int srv = socket(AF_UNIX, SOCK_DGRAM, 0);
    CHECK(srv >= 0 && bind(srv, (struct sockaddr *)&sa, salen) == 0, "bind an abstract name");
    int dup2nd = socket(AF_UNIX, SOCK_DGRAM, 0);
    CHECK(bind(dup2nd, (struct sockaddr *)&sa, salen) == -1 && errno == EADDRINUSE, "the same name again: EADDRINUSE");
    close(dup2nd);
    int cli = socket(AF_UNIX, SOCK_DGRAM, 0);
    CHECK(sendto(cli, "hi", 2, 0, (struct sockaddr *)&sa, salen) == 2, "sendto the name");
    flen = sizeof from;
    CHECK(recvfrom(srv, buf, sizeof buf, 0, (struct sockaddr *)&from, &flen) == 2, "recvfrom it");
    CHECK(flen <= sizeof(sa_family_t) || (flen > 2 && from.sun_path[0] == 0), "an unbound sender's address: family alone or an autobound abstract name");

    char path[64];
    snprintf(path, sizeof path, "/tmp/gex-dgram-%d", (int)getpid());
    unlink(path);
    struct sockaddr_un pa;
    socklen_t palen = addr(&pa, path, 0);
    CHECK(bind(cli, (struct sockaddr *)&pa, palen) == 0, "bind the sender to a path");
    sendto(cli, "again", 5, 0, (struct sockaddr *)&sa, salen);
    flen = sizeof from;
    recvfrom(srv, buf, sizeof buf, 0, (struct sockaddr *)&from, &flen);
    CHECK(!strcmp(from.sun_path, path), "a bound sender's address is its path");
    /* recvmsg: the sender in msg_name, MSG_TRUNC in msg_flags. */
    sendto(srv, "reply-long", 10, 0, (struct sockaddr *)&pa, palen);
    struct iovec iv = { buf, 5 };
    struct msghdr mh = { .msg_name = &from, .msg_namelen = sizeof from, .msg_iov = &iv, .msg_iovlen = 1 };
    ssize_t got = recvmsg(cli, &mh, 0);
    CHECK(got == 5 && (mh.msg_flags & MSG_TRUNC), "recvmsg: 5 bytes, MSG_TRUNC");
    CHECK(mh.msg_namelen == salen && from.sun_path[0] == 0 && !memcmp(from.sun_path + 1, name, strlen(name)),
          "recvmsg: from the abstract name");
    /* connect, then send and getpeername. */
    int c2 = socket(AF_UNIX, SOCK_DGRAM, 0);
    CHECK(connect(c2, (struct sockaddr *)&pa, palen) == 0, "connect to the path");
    CHECK(send(c2, "via", 3, 0) == 3 && recv(cli, buf, sizeof buf, 0) == 3, "send goes to it");
    flen = sizeof from;
    CHECK(getpeername(c2, (struct sockaddr *)&from, &flen) == 0 && !strcmp(from.sun_path, path), "getpeername is the path");
    flen = sizeof from;
    CHECK(getsockname(cli, (struct sockaddr *)&from, &flen) == 0 && !strcmp(from.sun_path, path), "getsockname is its own path");
    int unc = socket(AF_UNIX, SOCK_DGRAM, 0);
    CHECK(send(unc, "x", 1, 0) == -1, "send when not connected fails (%s)", strerror(errno));
    struct sockaddr_un none;
    socklen_t nlen = addr(&none, "/tmp/gex-dgram-none", 0);
    CHECK(sendto(unc, "x", 1, 0, (struct sockaddr *)&none, nlen) == -1 && errno == ENOENT, "a missing name: ENOENT");
    int lst = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un st;
    char spath[64];
    snprintf(spath, sizeof spath, "/tmp/gex-dgram-s-%d", (int)getpid());
    unlink(spath);
    socklen_t stlen = addr(&st, spath, 0);
    bind(lst, (struct sockaddr *)&st, stlen);
    listen(lst, 1);
    CHECK(sendto(unc, "x", 1, 0, (struct sockaddr *)&st, stlen) == -1, "a stream socket's name fails (%s)", strerror(errno));

    pid_t pid = fork();
    if (pid == 0) {
        int s = socket(AF_UNIX, SOCK_DGRAM, 0);
        _exit(sendto(s, "child", 5, 0, (struct sockaddr *)&sa, salen) == 5 ? 0 : 1);
    }
    int st2;
    waitpid(pid, &st2, 0);
    CHECK(WIFEXITED(st2) && WEXITSTATUS(st2) == 0 && recv(srv, buf, sizeof buf, 0) == 5 && !memcmp(buf, "child", 5),
          "a forked child sends to its parent's name");
    unlink(path);
    unlink(spath);
    return fail;
}
