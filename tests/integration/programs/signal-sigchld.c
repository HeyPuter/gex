/* A parent that polls with WNOHANG and sleeps in sigsuspend between polls.
 *
 * This is how coreutils `timeout` waits, and it needs the SIGCHLD a child's
 * exit owes it: nothing else will end the sigsuspend. gex published the exit
 * status and woke the condvar a blocking `wait4` parks on, which is why the
 * gap went unnoticed -- a parent that waits the other way sat forever for a
 * child that had already exited.
 */
#include <signal.h>
#include <stdio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t chld;
static void on_chld(int s) { (void)s; chld = 1; }

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_chld;
    sigaction(SIGCHLD, &sa, NULL);

    sigset_t block, orig, empty;
    sigemptyset(&block);
    sigaddset(&block, SIGCHLD);
    sigemptyset(&empty);
    /* Blocked across the fork so the child cannot exit before we are ready,
       which is the whole reason this dance exists. */
    sigprocmask(SIG_BLOCK, &block, &orig);

    pid_t p = fork();
    if (p == 0) {
        sigprocmask(SIG_SETMASK, &orig, NULL);
        /* Long enough that the parent is certainly inside sigsuspend when
           this exits. Without the pause the child can be gone before the
           first WNOHANG poll, the suspend is never entered, and whether the
           handler ran is a race -- which is what the first version of this
           test measured. */
        usleep(400000);
        _exit(23);
    }

    time_t t0 = time(NULL);
    int st = 0;
    int polls = 0;
    while (waitpid(p, &st, WNOHANG) == 0) {
        polls++;
        if (time(NULL) - t0 > 20) {
            printf("gave up: no SIGCHLD\n");
            return 1;
        }
        sigsuspend(&empty);
    }
    printf("reaped exit=%d chld=%d waited=%s\n",
           WIFEXITED(st) ? WEXITSTATUS(st) : -1, chld, polls > 0 ? "yes" : "no");
    return 0;
}
