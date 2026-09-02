/*
 * Does a process still reap its children after `setsid`?
 *
 * A terminal emulator's child calls setsid to become a session leader with the
 * pty as its controlling terminal, and everything it runs is a child of that.
 * If the session change loses the parent-child link, the shell in the terminal
 * runs exactly one command and then waits forever -- which looks like a
 * terminal that printed one line and froze, with nothing anywhere saying why.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int one(const char *how)
{
    pid_t pid = fork();
    if (pid < 0) {
        printf("%s: fork: %s\n", how, strerror(errno));
        return 1;
    }
    if (pid == 0) {
        _exit(7);
    }
    printf("%s: pid %d ppid %d sid %d pgid %d, child %d\n", how,
           (int) getpid(), (int) getppid(), (int) getsid(0), (int) getpgid(0),
           (int) pid);
    fflush(stdout);
    int status = 0;
    pid_t got = waitpid(pid, &status, 0);
    if (got != pid) {
        printf("%s: wait returned %d (%s) -- NEVER REAPED\n", how, (int) got,
               strerror(errno));
        return 1;
    }
    printf("%s: reaped %d, status %d\n", how, (int) got, WEXITSTATUS(status));
    return 0;
}

int main(void)
{
    int bad = 0;
    bad |= one("before setsid");
    fflush(stdout);
    if (setsid() < 0)
        printf("setsid: %s\n", strerror(errno));
    else
        printf("setsid: now session %d\n", (int) getsid(0));
    fflush(stdout);
    bad |= one("after setsid");
    printf("setsid-wait: %s\n", bad ? "A WAIT DID NOT RETURN" : "both waits returned");
    return bad;
}
