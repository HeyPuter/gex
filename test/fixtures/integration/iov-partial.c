/* A vectored read that fills one buffer and finds the next empty: `readv` must
 * report the bytes it already took, not the `EAGAIN` from the empty second
 * vector. `writev` has the mirror image -- a partial write must be reported,
 * not turned into an error the caller retries. */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>
#include <fcntl.h>

int main(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) {
        printf("socketpair failed\n");
        return 1;
    }
    /* Non-blocking, so the empty second vector reports EAGAIN rather than
       waiting -- which is what a server's socket looks like. */
    fcntl(sv[0], F_SETFL, O_NONBLOCK);

    const char *msg = "HEADER..";           /* exactly the first vector */
    write(sv[1], msg, 8);

    char a[8], b[64];
    struct iovec v[2] = {{a, sizeof a}, {b, sizeof b}};
    ssize_t n = readv(sv[0], v, 2);
    printf("readv = %zd", n);
    if (n < 0) printf(" errno=%d", errno);
    printf("\n");
    if (n == 8) printf("first vector: %.8s\n", a);

    /* Nothing may be left behind: a second read must find the socket empty. */
    char c[8];
    ssize_t m = read(sv[0], c, sizeof c);
    printf("leftover = %zd errno=%d\n", m, m < 0 ? errno : 0);

    /* And the writev direction, with a vector list longer than the pipe. */
    int p[2];
    if (pipe(p) != 0) return 1;
    fcntl(p[1], F_SETFL, O_NONBLOCK);
    static char big[512 * 1024];
    memset(big, 'x', sizeof big);
    struct iovec w[2] = {{big, 4096}, {big, sizeof big}};
    ssize_t k = writev(p[1], w, 2);
    printf("writev %s\n", k >= 4096 ? "reported its partial write" : "lost bytes");
    return 0;
}
