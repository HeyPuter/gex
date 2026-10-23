// A branch into data is SIGSEGV: for the program's handler when it has one,
// else the death of the whole process (a thread of it still running), which
// its parent sees as a signal.
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static unsigned data[64] = { 0xffffffff };

static void handler(int sig, siginfo_t *si, void *uc) {
    (void)uc;
    char line[80];
    int n = snprintf(line, sizeof line, "handler: signal %d at data %d\n", sig, si->si_addr == (void *)data);
    write(1, line, n);
    _exit(42);
}

static void *sleeper(void *arg) {
    (void)arg;
    for (;;) pause();
}

static void jump(void) {
    void (*f)(void) = (void (*)(void))(void *)data;
    f();
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "handled")) {
        struct sigaction sa = { 0 };
        sa.sa_sigaction = handler;
        sa.sa_flags = SA_SIGINFO;
        sigaction(SIGSEGV, &sa, 0);
        jump();
        return 1;
    }
    if (argc > 1 && !strcmp(argv[1], "threaded")) {
        pthread_t t;
        pthread_create(&t, 0, sleeper, 0);
        jump();
        return 1;
    }
    for (int i = 0; i < 2; i++) {
        pid_t p = fork();
        if (p == 0) {
            execl(argv[0], argv[0], i ? "threaded" : "handled", (char *)0);
            _exit(127);
        }
        int st = 0;
        waitpid(p, &st, 0);
        if (WIFSIGNALED(st))
            printf("%s: killed by signal %d\n", i ? "threaded" : "handled", WTERMSIG(st));
        else
            printf("%s: exited %d\n", i ? "threaded" : "handled", WEXITSTATUS(st));
        fflush(stdout);
    }
    return 0;
}
