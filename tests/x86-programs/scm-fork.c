/* Descriptors passed between processes (SCM_RIGHTS over a socketpair a fork
 * shares): a file opened by path, with its offset, and a pipe's write end,
 * sent parent to child; the child reads the file from the offset and writes
 * to the pipe. Between processes the socket lives in the global kernel and
 * each descriptor travels as a description rebuilt in the receiver (the
 * lock-free recvmsg rebuilds it through the process's published namespace,
 * docs/kernel-locking.md L5). */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static int send_fds(int s, int a, int b) {
    char byte = 'x';
    struct iovec iov = {&byte, 1};
    char ctl[CMSG_SPACE(2 * sizeof(int))];
    memset(ctl, 0, sizeof ctl);
    struct msghdr mh = {0};
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    mh.msg_control = ctl;
    mh.msg_controllen = sizeof ctl;
    struct cmsghdr *c = CMSG_FIRSTHDR(&mh);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(2 * sizeof(int));
    int fds[2] = {a, b};
    memcpy(CMSG_DATA(c), fds, sizeof fds);
    return sendmsg(s, &mh, 0) == 1 ? 0 : -1;
}

static int recv_fds(int s, int *a, int *b) {
    char byte;
    struct iovec iov = {&byte, 1};
    char ctl[CMSG_SPACE(2 * sizeof(int))];
    struct msghdr mh = {0};
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    mh.msg_control = ctl;
    mh.msg_controllen = sizeof ctl;
    if (recvmsg(s, &mh, 0) != 1) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&mh);
    if (!c || c->cmsg_type != SCM_RIGHTS || c->cmsg_len != CMSG_LEN(2 * sizeof(int))) return -1;
    int fds[2];
    memcpy(fds, CMSG_DATA(c), sizeof fds);
    *a = fds[0];
    *b = fds[1];
    return 0;
}

int main(void) {
    /* A name of its own: concurrent runs under gexc share /tmp. */
    char path[] = "/tmp/scm-fork-XXXXXX";
    int f = mkstemp(path);
    if (f < 0 || write(f, "skip:payload", 12) != 12) {
        perror("file");
        return 1;
    }
    lseek(f, 5, SEEK_SET);
    int sv[2], p[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) || pipe(p)) {
        perror("socketpair");
        return 1;
    }
    pid_t pid = fork();
    if (pid == 0) {
        close(sv[0]);
        close(p[0]);
        close(p[1]);
        close(f);
        int rf, wp;
        if (recv_fds(sv[1], &rf, &wp)) _exit(2);
        char buf[16] = {0};
        int n = read(rf, buf, sizeof buf - 1);
        char out[32];
        int m = snprintf(out, sizeof out, "%d:%s", n, buf);
        _exit(write(wp, out, m) == m ? 0 : 3);
    }
    close(sv[1]);
    if (send_fds(sv[0], f, p[1])) {
        perror("sendmsg");
        return 1;
    }
    close(p[1]);
    char got[32] = {0};
    int n = read(p[0], got, sizeof got - 1);
    int st;
    waitpid(pid, &st, 0);
    printf("child read through passed fds: %s (%d bytes), exit %d\n", n > 0 ? got : "nothing", n, WEXITSTATUS(st));
    unlink(path);
    return 0;
}
