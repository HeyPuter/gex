/*
 * `vfork` + `exec` + `wait`, which is how a shell runs a command.
 *
 * dash uses vfork (clone with CLONE_VM|CLONE_VFORK) for every external
 * command, so if the parent does not come back from the wait, a script runs
 * exactly one command and then stops -- which from the outside looks like a
 * terminal that printed one line and froze.
 *
 * Each step prints, and both spellings are tried: fork first (the one gex has
 * always had) and then vfork, so a difference between them is visible in one
 * run.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int run(const char *how, pid_t pid, const char *cmd)
{
    if (pid < 0) {
        printf("%s: failed: %s\n", how, strerror(errno));
        return 1;
    }
    if (pid == 0) {
        execl("/bin/sh", "sh", "-c", cmd, (char *) NULL);
        _exit(127);
    }
    printf("%s: child %d started, waiting\n", how, pid);
    fflush(stdout);
    int status = 0;
    pid_t got = waitpid(pid, &status, 0);
    if (got != pid) {
        printf("%s: wait returned %d (%s) -- the child was never reaped\n",
               how, (int) got, strerror(errno));
        return 1;
    }
    printf("%s: child %d exited %d\n", how, pid, WEXITSTATUS(status));
    return 0;
}

int main(void)
{
    int bad = 0;
    bad |= run("fork", fork(), "exit 3");
    fflush(stdout);
    bad |= run("vfork", vfork(), "exit 4");
    fflush(stdout);
    /* And again with something that takes time to run, since a wait that
     * returns only because the child was already gone proves less. */
    bad |= run("vfork+sleep", vfork(), "/bin/sleep 1; exit 5");
    printf("vfork-wait: %s\n", bad ? "SOMETHING HUNG OR FAILED" : "all three waits returned");
    return bad;
}
