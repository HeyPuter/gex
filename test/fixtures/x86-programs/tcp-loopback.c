/* TCP over the loopback between guests: a listener on 127.0.0.1 (an
 * ephemeral port, then the same port again: EADDRINUSE), a connection
 * from a thread and one from a forked child, data both ways, end of file
 * on close, a non-blocking accept with nothing waiting, and the listener
 * readable in poll once a connection waits. Prints nothing that depends on
 * which port. */
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static struct sockaddr_in where;

static void *client(void *arg) {
    (void)arg;
    int c = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(c, (struct sockaddr *)&where, sizeof where) != 0) {
        printf("thread connect: errno %d\n", errno);
        return 0;
    }
    const char *msg = "hello from the thread";
    write(c, msg, strlen(msg));
    char buf[64] = {0};
    ssize_t n = read(c, buf, sizeof buf - 1);
    printf("thread got %zd [%s]\n", n, buf);
    close(c);
    return 0;
}

int main(void) {
    int l = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(l, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_port = 0, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
    printf("bind %d\n", bind(l, (struct sockaddr *)&a, sizeof a));
    printf("listen %d\n", listen(l, 8));
    socklen_t al = sizeof where;
    getsockname(l, (struct sockaddr *)&where, &al);
    printf("listening port nonzero %d family %d\n", where.sin_port != 0, where.sin_family);

    int l2 = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in same = where;
    int r = bind(l2, (struct sockaddr *)&same, sizeof same);
    int e = errno;
    if (r == 0) {
        r = listen(l2, 1);
        e = errno;
    }
    printf("second listener on the port: %d %s\n", r, r ? (e == EADDRINUSE ? "EADDRINUSE" : "other") : "");
    close(l2);

    fcntl(l, F_SETFL, O_NONBLOCK);
    errno = 0;
    printf("nonblocking accept with none waiting: %d %s\n", accept(l, 0, 0), errno == EAGAIN ? "EAGAIN" : "other");
    fcntl(l, F_SETFL, 0);

    pthread_t t;
    pthread_create(&t, 0, client, 0);
    struct pollfd p = {.fd = l, .events = POLLIN};
    printf("poll: %d revents-in %d\n", poll(&p, 1, 5000), !!(p.revents & POLLIN));
    struct sockaddr_in peer;
    socklen_t pl = sizeof peer;
    int s = accept(l, (struct sockaddr *)&peer, &pl);
    printf("accepted %d from 127.0.0.1 %d\n", s >= 0, peer.sin_addr.s_addr == htonl(INADDR_LOOPBACK));
    char buf[64] = {0};
    ssize_t n = read(s, buf, sizeof buf - 1);
    printf("server got %zd [%s]\n", n, buf);
    write(s, "pong", 4);
    pthread_join(t, 0);
    n = read(s, buf, sizeof buf);
    printf("after the client closed: read %zd\n", n);
    close(s);

    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        int k = socket(AF_INET, SOCK_STREAM, 0);
        int ok = connect(k, (struct sockaddr *)&where, sizeof where);
        dprintf(k, "child %d", ok);
        char b[16] = {0};
        read(k, b, sizeof b - 1);
        printf("child got [%s]\n", b);
        fflush(stdout);
        _exit(0);
    }
    s = accept(l, 0, 0);
    memset(buf, 0, sizeof buf);
    n = read(s, buf, sizeof buf - 1);
    printf("from the child %zd [%s]\n", n, buf);
    write(s, "bye", 3);
    int st;
    waitpid(c, &st, 0);
    close(s);
    close(l);
    /* The port is free again. */
    int l3 = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(l3, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    printf("rebind after close %d listen %d\n", bind(l3, (struct sockaddr *)&where, sizeof where), listen(l3, 1));
    return 0;
}
