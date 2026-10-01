/* A write to a pipe with no reader raises SIGPIPE; its default action kills
 * the writer (a shell reports status 141), and with the signal ignored or
 * handled the write fails with EPIPE. send() with MSG_NOSIGNAL fails
 * without it. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile int caught;
static void handler(int sig) { caught = sig; }

int main(void) {
    int p[2];
    setvbuf(stdout, 0, _IONBF, 0);
    /* Default action: the child dies of SIGPIPE at its write. */
    pid_t c = fork();
    if (c == 0) {
        pipe(p);
        close(p[0]);
        write(p[1], "x", 1);
        puts("survived");
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    printf("default: %s %d\n", WIFSIGNALED(st) ? "killed by" : "exited", WIFSIGNALED(st) ? WTERMSIG(st) : WEXITSTATUS(st));
    signal(SIGPIPE, SIG_IGN);
    pipe(p);
    close(p[0]);
    long r = write(p[1], "x", 1);
    printf("ignored: write %ld errno %s\n", r, r < 0 ? strerror(errno) : "-");
    close(p[1]);

    signal(SIGPIPE, handler);
    pipe(p);
    close(p[0]);
    r = write(p[1], "x", 1);
    printf("handled: write %ld errno %s caught %d\n", r, r < 0 ? strerror(errno) : "-", caught);
    close(p[1]);

    caught = 0;
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    close(sv[1]);
    r = send(sv[0], "x", 1, MSG_NOSIGNAL);
    printf("MSG_NOSIGNAL: send %ld errno %s caught %d\n", r, r < 0 ? strerror(errno) : "-", caught);
    r = send(sv[0], "x", 1, 0);
    printf("send: %ld errno %s caught %d\n", r, r < 0 ? strerror(errno) : "-", caught);
    return 0;
}
