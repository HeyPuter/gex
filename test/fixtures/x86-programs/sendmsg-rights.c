/* sendmsg/recvmsg over a unix socketpair with two iovecs and a descriptor
 * passed as SCM_RIGHTS, and sendmmsg/recvmmsg: the message structures are
 * pointer-sized, so an i386 process's layout differs from x86-64's. */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

int main(void) {
    int sv[2], pipefd[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) || pipe(pipefd)) return 1;

    char a[] = "hello ", b[] = "world";
    struct iovec iov[2] = {{a, 6}, {b, 5}};
    union { struct cmsghdr h; char buf[CMSG_SPACE(sizeof(int))]; } cm;
    memset(&cm, 0, sizeof cm);
    struct msghdr msg = {0};
    msg.msg_iov = iov;
    msg.msg_iovlen = 2;
    msg.msg_control = cm.buf;
    msg.msg_controllen = sizeof cm.buf;
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &pipefd[1], sizeof(int));
    printf("sent %zd\n", sendmsg(sv[0], &msg, 0));

    char r1[4], r2[16];
    struct iovec riov[2] = {{r1, 4}, {r2, sizeof r2}};
    union { struct cmsghdr h; char buf[CMSG_SPACE(sizeof(int))]; } rcm;
    memset(&rcm, 0, sizeof rcm);
    struct msghdr rmsg = {0};
    rmsg.msg_iov = riov;
    rmsg.msg_iovlen = 2;
    rmsg.msg_control = rcm.buf;
    rmsg.msg_controllen = sizeof rcm.buf;
    ssize_t n = recvmsg(sv[1], &rmsg, 0);
    printf("received %zd: %.4s|%.*s\n", n, r1, (int)(n > 4 ? n - 4 : 0), r2);
    struct cmsghdr *rc = CMSG_FIRSTHDR(&rmsg);
    if (rc && rc->cmsg_level == SOL_SOCKET && rc->cmsg_type == SCM_RIGHTS &&
        rmsg.msg_controllen >= CMSG_LEN(sizeof(int))) {
        int fd;
        memcpy(&fd, CMSG_DATA(rc), sizeof fd);
        printf("fd passed, controllen ok %d\n", rmsg.msg_controllen == CMSG_SPACE(sizeof(int)) || rmsg.msg_controllen == CMSG_LEN(sizeof(int)));
        write(fd, "via passed fd", 13);
        close(fd);
        char p[32] = {0};
        read(pipefd[0], p, sizeof p - 1);
        printf("pipe: %s\n", p);
    } else {
        printf("no descriptor\n");
    }

    /* sendmmsg, then recvmmsg of one message, on the stream pair (gex's
     * unix sockets keep no datagram boundaries yet). */
    char m1[] = "one", m2[] = "three";
    struct iovec s1 = {m1, 3}, s2 = {m2, 5};
    struct mmsghdr out[2];
    memset(out, 0, sizeof out);
    out[0].msg_hdr.msg_iov = &s1; out[0].msg_hdr.msg_iovlen = 1;
    out[1].msg_hdr.msg_iov = &s2; out[1].msg_hdr.msg_iovlen = 1;
    printf("sendmmsg %d: %u %u\n", sendmmsg(sv[0], out, 2, 0), out[0].msg_len, out[1].msg_len);
    char g1[16];
    struct iovec q1 = {g1, sizeof g1};
    struct mmsghdr in[1];
    memset(in, 0, sizeof in);
    in[0].msg_hdr.msg_iov = &q1; in[0].msg_hdr.msg_iovlen = 1;
    int got = recvmmsg(sv[1], in, 1, 0, NULL);
    printf("recvmmsg %d: %u %.*s\n", got, in[0].msg_len, (int)in[0].msg_len, g1);
    return 0;
}
