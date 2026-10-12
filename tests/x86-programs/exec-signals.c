/* What signal state survives execve (POSIX): the mask, SIG_IGN dispositions
 * and pending signals do; a caught signal's handler is reset to SIG_DFL.
 * The page once replaced the whole signal state on exec, mask and ignored
 * signals included. The program execs itself (argv[0], else
 * /proc/self/exe) with an argument, and the child reports. In the page,
 * run.sh bundles it into the guest's filesystem under the argv[0] it gives. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static void on_term(int s) { (void)s; }

static const char *disp(int sig) {
    struct sigaction sa;
    sigaction(sig, NULL, &sa);
    return sa.sa_handler == SIG_IGN ? "ignored" : sa.sa_handler == SIG_DFL ? "default" : "caught";
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "child") == 0) {
        sigset_t mask, pend;
        sigprocmask(SIG_BLOCK, NULL, &mask);
        sigpending(&pend);
        printf("mask keeps SIGUSR1: %s\n", sigismember(&mask, SIGUSR1) ? "yes" : "no");
        printf("mask keeps SIGUSR2 unblocked: %s\n", sigismember(&mask, SIGUSR2) ? "no" : "yes");
        printf("SIGPIPE: %s\n", disp(SIGPIPE));
        printf("SIGINT: %s\n", disp(SIGINT));
        printf("SIGTERM (was caught): %s\n", disp(SIGTERM));
        printf("SIGUSR1 still pending: %s\n", sigismember(&pend, SIGUSR1) ? "yes" : "no");
        return 0;
    }
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigprocmask(SIG_BLOCK, &s, NULL);
    signal(SIGPIPE, SIG_IGN);
    signal(SIGINT, SIG_IGN);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_term;
    sigaction(SIGTERM, &sa, NULL);
    raise(SIGUSR1);
    fflush(stdout);
    char *args[] = {argv[0], "child", NULL};
    execv(argv[0], args);
    execv("/proc/self/exe", args);
    perror("execv");
    return 1;
}
