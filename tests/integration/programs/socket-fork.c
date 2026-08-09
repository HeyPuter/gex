/* A unix socket whose two ends are in different processes.
 *
 * Which is what a unix socket is *for*, and what stopped working the moment
 * each process got a Worker of its own: the name table a server binds in is
 * Rust data in its own address space, and the client is somewhere else
 * entirely. An X server bound /tmp/.X11-unix/X0 and sat at "0 waiting" while
 * its client, two forks away, looked in a table that had never heard of it.
 *
 * So: bind and listen, fork, have the child connect and speak, and have the
 * parent accept and answer. Every line is ordered by an event -- an accept, a
 * read, a write, an exit status -- and never by which process happens to be
 * running, because with real Workers that order is not fixed.
 */
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#define PATH "/tmp/gex-socket-fork"

static void put(const char *s) { write(1, s, strlen(s)); }

static void die(const char *what) {
    put("FAIL ");
    put(what);
    put("\n");
    _exit(1);
}

int main(void) {
    unlink(PATH);

    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    if (srv < 0) die("socket");
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    strcpy(a.sun_path, PATH);
    if (bind(srv, (struct sockaddr *)&a, sizeof a) != 0) die("bind");
    if (listen(srv, 4) != 0) die("listen");
    put("server: listening\n");

    pid_t kid = fork();
    if (kid < 0) die("fork");
    if (kid == 0) {
        /* The child is a different process and, in a browser, a different
         * address space -- so this connect is the whole point of the test. */
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        if (c < 0) die("child socket");
        struct sockaddr_un to = {.sun_family = AF_UNIX};
        strcpy(to.sun_path, PATH);
        if (connect(c, (struct sockaddr *)&to, sizeof to) != 0) die("connect");
        if (write(c, "ping", 4) != 4) die("child write");
        char buf[16] = {0};
        ssize_t n = read(c, buf, sizeof buf - 1);
        if (n <= 0) die("child read");
        put("child: got ");
        put(buf);
        put("\n");
        _exit(strcmp(buf, "pong") == 0 ? 0 : 1);
    }

    int s = accept(srv, NULL, NULL);
    if (s < 0) die("accept");
    put("server: accepted\n");
    char buf[16] = {0};
    ssize_t n = read(s, buf, sizeof buf - 1);
    if (n <= 0) die("server read");
    put("server: got ");
    put(buf);
    put("\n");
    if (write(s, "pong", 4) != 4) die("server write");

    int status = 0;
    if (waitpid(kid, &status, 0) != kid) die("waitpid");
    printf("child exited %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 1;
}
