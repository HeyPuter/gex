/* shutdown(2) on a unix socketpair: after SHUT_WR the peer polls readable
 * and reads end-of-file (wineserver's sock_check_pollhup), its own writes
 * still go through the other way; after SHUT_RD the peer's writes fail. */
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void) {
    int sv[2];
    char c;
    signal(SIGPIPE, SIG_IGN);
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv)) return 1;
    printf("shutdown(SHUT_WR) = %d\n", shutdown(sv[0], SHUT_WR));
    struct pollfd p = { sv[1], POLLIN, 0 };
    int n = poll(&p, 1, 1);
    printf("peer poll = %d, POLLIN %d\n", n, !!(p.revents & POLLIN));
    printf("peer read = %zd\n", read(sv[1], &c, 1));
    printf("peer write back = %zd\n", write(sv[1], "x", 1));
    printf("read it = %zd\n", read(sv[0], &c, 1));
    ssize_t w = write(sv[0], "y", 1);
    printf("write after SHUT_WR = %zd (EPIPE %d)\n", w, errno == EPIPE);
    int t[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, t);
    printf("shutdown(SHUT_RD) = %d\n", shutdown(t[0], SHUT_RD));
    errno = 0;
    w = write(t[1], "z", 1);
    printf("peer write after SHUT_RD = %zd (EPIPE %d)\n", w, errno == EPIPE);
    int r = shutdown(t[0], 7);
    printf("shutdown(bad how) = %d (EINVAL %d)\n", r, errno == EINVAL);
    return 0;
}
