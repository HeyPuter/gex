/* Process descriptors polled while other processes collect children, as a
 * KDE session does: QProcess polls a pidfd of each child it starts, and
 * shells wait4 for theirs. Neither may hold up the other (a poll of a
 * pidfd lists the process table; a wait4 that collects takes it too).
 * Three pollers poll a running process's pidfd until told to stop, three
 * collectors each start and wait4 for 200 children; pidfdwait.sh limits
 * the time, so a freeze fails rather than hangs. One line per worker. */
#define _GNU_SOURCE
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define NR_PIDFD_OPEN 434
#define WORKERS 3
#define CHILDREN 200

static int poller(pid_t target, int stop)
{
    int fd = (int)syscall(NR_PIDFD_OPEN, target, 0);
    if (fd < 0) {
        printf("poller: pidfd_open failed\n");
        return 1;
    }
    long polls = 0;
    for (;;) {
        struct pollfd p[2] = { { .fd = fd, .events = POLLIN }, { .fd = stop, .events = POLLIN } };
        if (poll(p, 2, 0) < 0) return 1;
        if (p[0].revents & POLLIN) {
            printf("poller: target exited early\n");
            return 1;
        }
        if (p[1].revents) break;
        polls++;
    }
    printf("poller: target running %s\n", polls > 0 ? "throughout" : "(no polls)");
    return 0;
}

static int collector(void)
{
    for (int k = 0; k < CHILDREN; k++) {
        pid_t c = fork();
        if (c == 0) _exit(k & 0x7f);
        int st;
        if (waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st) != (k & 0x7f)) {
            printf("collector: child %d lost\n", k);
            return 1;
        }
    }
    printf("collector: %d children collected\n", CHILDREN);
    return 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IONBF, 0);
    int hold[2], stop[2];
    if (pipe(hold)) return 2;
    pid_t target = fork();
    if (target == 0) {
        char c;
        close(hold[1]);
        (void)!read(hold[0], &c, 1);
        _exit(0);
    }
    close(hold[0]);
    if (pipe(stop)) return 2;
    pid_t pw[WORKERS], cw[WORKERS];
    for (int i = 0; i < WORKERS; i++) {
        pw[i] = fork();
        if (pw[i] == 0) {
            close(stop[1]);
            _exit(poller(target, stop[0]));
        }
    }
    close(stop[0]);
    for (int i = 0; i < WORKERS; i++) {
        cw[i] = fork();
        if (cw[i] == 0) _exit(collector());
    }
    int fail = 0, st;
    for (int i = 0; i < WORKERS; i++) {
        waitpid(cw[i], &st, 0);
        fail |= !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    close(stop[1]);
    for (int i = 0; i < WORKERS; i++) {
        waitpid(pw[i], &st, 0);
        fail |= !WIFEXITED(st) || WEXITSTATUS(st) != 0;
    }
    close(hold[1]);
    waitpid(target, &st, 0);
    return fail;
}
