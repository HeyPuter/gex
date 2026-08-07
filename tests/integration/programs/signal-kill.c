/* Does a fatal signal from another process kill it?
 *
 * `timeout` sends SIGTERM to its child and then waits. If the child does not
 * die, the wait never ends -- which is what `timeout 3 sleep 30` does now
 * that the timer itself works. */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    /* 1. A child asleep in nanosleep, killed by its parent. */
    pid_t p = fork();
    if (p == 0) { sleep(30); _exit(7); }
    sleep(1);
    kill(p, SIGTERM);
    int st = 0;
    pid_t r = waitpid(p, &st, 0);
    printf("blocked child: r=%s signalled=%d exit=%d\n", r == p ? "ok" : "BAD",
           WIFSIGNALED(st) ? WTERMSIG(st) : 0,
           WIFEXITED(st) ? WEXITSTATUS(st) : -1);

    /* 2. A child spinning, so the signal lands at a back-edge not a syscall. */
    p = fork();
    if (p == 0) { for (;;) { } }
    sleep(1);
    kill(p, SIGTERM);
    r = waitpid(p, &st, 0);
    printf("spinning child: r=%s signalled=%d\n", r == p ? "ok" : "BAD",
           WIFSIGNALED(st) ? WTERMSIG(st) : 0);

    /* 3. kill(0, sig) goes to the whole process group, which is what
       coreutils `timeout` does to re-raise on itself. */
    printf("done\n");
    return 0;
}
