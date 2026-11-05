/* An internet socket's last descriptor going away closes it, however it
 * goes: dup2 over it, close_range, an execve closing it on exec, as well as
 * close. The peer reads EOF and a listener's port is free again. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(name, cond)                                                                \
    do {                                                                                 \
        printf("%s %s\n", (cond) ? "ok  " : "FAIL", name);                               \
        fflush(stdout);                                                                  \
    } while (0)

static int listener(int *port) {
    int l = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
                            .sin_port = htons(*port)};
    if (bind(l, (struct sockaddr *)&a, sizeof a) || listen(l, 4)) {
        close(l);
        return -1;
    }
    socklen_t al = sizeof a;
    getsockname(l, (struct sockaddr *)&a, &al);
    *port = ntohs(a.sin_port);
    return l;
}

/* A connected pair: *client and *server. */
static void connected(int *client, int *server) {
    int port = 0, l = listener(&port);
    *client = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    struct sockaddr_in a = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK),
                            .sin_port = htons(port)};
    connect(*client, (struct sockaddr *)&a, sizeof a);
    *server = accept4(l, 0, 0, SOCK_CLOEXEC);
    close(l);
}

/* The client sees EOF within two seconds. */
static int eof_soon(int fd) {
    struct pollfd p = {fd, POLLIN, 0};
    if (poll(&p, 1, 2000) != 1) return 0;
    char b[8];
    return read(fd, b, sizeof b) == 0;
}

int main(int argc, char **argv) {
    if (argc > 1) { /* exec'd: hold on a while, then go */
        usleep(3000000);
        return 0;
    }
    int c, s;
    connected(&c, &s);
    CHECK("connected", c >= 0 && s >= 0 && write(s, "x", 1) == 1);
    char b[8];
    read(c, b, 1);

    int devnull = open("/dev/null", O_RDONLY | O_CLOEXEC);
    dup2(devnull, s); /* s's socket had only this descriptor */
    CHECK("dup2 over the last descriptor: peer reads EOF", eof_soon(c));
    close(c);
    close(s);

    connected(&c, &s);
    int hi = dup2(s, 40);
    close(s);
    syscall(SYS_close_range, 40, 40, 0);
    CHECK("close_range on the last descriptor: peer reads EOF", hi == 40 && eof_soon(c));
    close(c);

    connected(&c, &s);
    pid_t pid = fork();
    if (pid == 0) {
        close(c);
        /* s is close-on-exec; the program we become holds nothing */
        execl(argv[0], argv[0], "hold", (char *)0);
        _exit(127);
    }
    close(s);
    CHECK("exec closing the last descriptor: peer reads EOF", eof_soon(c));
    int st;
    waitpid(pid, &st, 0);
    close(c);

    /* a listener's port comes free when dup2 replaces its descriptor */
    int port = 0, l = listener(&port);
    dup2(devnull, l);
    int l2 = listener(&port);
    CHECK("dup2 over a listener frees its port", l2 >= 0);
    return 0;
}
