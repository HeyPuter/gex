#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
int main(void) {
    for (int how = 0; how < 4; how++) {
        pid_t p = fork();
        if (p == 0) {
            if (how == 0) { int fd[2]; pipe(fd); close(fd[0]); write(fd[1], "x", 1); }
            if (how == 1) abort();
            if (how == 2) raise(SIGTERM);
            if (how == 3) raise(SIGSEGV);
            _exit(0);
        }
        if (p > 0 && how == 3) {
            /* and one another process kills */
            pid_t q = fork();
            if (q == 0) { for (;;) pause(); }
            kill(q, SIGKILL);
            int st = 0;
            waitpid(q, &st, 0);
            printf("child 4: pid %d signal %d\n", q, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
        }
        int st = 0;
        waitpid(p, &st, 0);
        printf("child %d: pid %d signal %d\n", how, p, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
        fflush(stdout);
    }
    return 0;
}
