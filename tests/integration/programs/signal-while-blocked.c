/* A signal that arrives while the program is blocked in a syscall.
 *
 * This is the ordinary case and it was broken in a browser tab for as long as
 * there has been one, in a way nothing here caught: a handler ran on top of
 * the wrong frame and the run ended by branching into unmapped memory.
 *
 * Two things had to be wrong at once. Finishing a parked syscall can deliver
 * a signal, and delivery reports where to go by returning `Resume(pc)` --
 * which the browser's scheduler discarded, resuming the instruction after the
 * `svc` with the handler's frame already built underneath it. And a handler
 * with no `sa_restorer` returns to a reserved address that deliberately has
 * nothing translated at it, which is how the host is told to `sigreturn`; the
 * browser had no case for that address and reported `nothing to translate`.
 *
 * The reason it went unnoticed for so long is that the first handler most
 * programs install is SIGCHLD, so it took a child exiting to reach one -- and
 * every test with a child here was a program that did not handle SIGCHLD.
 * `(echo sub); echo after` in a page printed `sub` and then died.
 *
 * So this blocks in `wait4` and takes SIGCHLD there.
 *
 * A timer would be the other half of this and is deliberately not here: a
 * signal cannot yet interrupt a blocked `read` in a browser tab, because the
 * scheduler only picks a parked thread when the thing it is parked on becomes
 * ready, and an alarm does not make a pipe readable. That has its own task.
 */
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t children;

static void on_child(int sig) {
    (void)sig;
    children++;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_child;
    sigaction(SIGCHLD, &sa, NULL);

    /* And a child exiting while the parent is inside `wait4`. The status has
     * to survive the handler: a `wait4` that loses its child to the signal
     * frame reports the wrong thing, or nothing. */
    pid_t kid = fork();
    if (kid == 0) {
        _exit(3);
    }
    int status = 0;
    pid_t got = waitpid(kid, &status, 0);
    printf("waited for %s, exit %d, sigchld %d\n", got == kid ? "the child" : "someone else",
           WIFEXITED(status) ? WEXITSTATUS(status) : -1, (int)children);

    /* Still running afterwards, which is the part that used to branch into
     * nothing. */
    puts("still here");
    return 0;
}
