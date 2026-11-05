/* `wait` after the parent has moved itself into a new process group, the shape
 * coreutils `timeout` has: put itself in its own group so it can signal the
 * job, block the signals it handles, fork, and poll with WNOHANG. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void put(const char *s) { write(1, s, strlen(s)); }

int main(void) {
    /* A group of its own, as `timeout` does so that it can signal the job. */
    if (setpgid(0, 0) != 0) put("note: setpgid failed\n");

    sigset_t block, old;
    sigemptyset(&block);
    sigaddset(&block, SIGCHLD);
    sigaddset(&block, SIGTERM);
    sigprocmask(SIG_BLOCK, &block, &old);

    pid_t kid = fork();
    if (kid < 0) {
        put("fork failed\n");
        return 1;
    }
    if (kid == 0) {
        sigprocmask(SIG_SETMASK, &old, NULL);
        /* Long enough that the parent's wait has to actually wait. */
        struct timespec t = {.tv_sec = 0, .tv_nsec = 200 * 1000 * 1000};
        nanosleep(&t, NULL);
        _exit(7);
    }

    /* WNOHANG in a loop: the answer to "nothing is ready yet" is *zero*, not
     * ECHILD. ECHILD means no such child, which the caller treats as an error. */
    put("parent: polling\n");
    int status = 0;
    pid_t got;
    int polls = 0;
    while ((got = waitpid(-1, &status, WNOHANG)) == 0) {
        if (++polls > 100000) {
            put("parent: the child never finished\n");
            return 1;
        }
        struct timespec t = {.tv_sec = 0, .tv_nsec = 1000 * 1000};
        nanosleep(&t, NULL);
    }
    if (got < 0) {
        printf("parent: waitpid failed with errno %d after %d polls\n", errno, polls);
        return 1;
    }
    printf("parent: collected %d after polling %d, exited %d\n", got == kid,
           polls > 0, WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    return WIFEXITED(status) && WEXITSTATUS(status) == 7 ? 0 : 1;
}
