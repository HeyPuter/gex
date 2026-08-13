/* A signal that arrives while the program is blocked in a syscall: block in
 * `wait4`, take SIGCHLD there, and check the wait's result survives the handler
 * frame and that the process keeps running afterwards. */
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

    /* A child exiting while the parent is inside `wait4`: the status must
     * survive the handler frame. */
    pid_t kid = fork();
    if (kid == 0) {
        _exit(3);
    }
    int status = 0;
    pid_t got = waitpid(kid, &status, 0);
    printf("waited for %s, exit %d, sigchld %d\n", got == kid ? "the child" : "someone else",
           WIFEXITED(status) ? WEXITSTATUS(status) : -1, (int)children);

    puts("still here");
    return 0;
}
