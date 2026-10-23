/* A vfork child that spawns (Qt's QProcess::startDetached: vfork, and in the
   child vforkfd, a CLONE_VM | CLONE_VFORK clone, then exec): the grandchild
   runs and the parent hears from it. musl's vfork is a fork on AArch64, so
   the child is cloned as glibc's vfork clones it. */
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int child(void *a) {
    char **args = a;
    pid_t g;
    /* by /proc/self/exe: argv[0] is a host path natively */
    int r = posix_spawn(&g, "/proc/self/exe", NULL, NULL, args, environ);
    _exit(r == 0 ? 0 : 100 + r);
}

int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "grand")) {
        int fd = atoi(argv[2]);
        write(fd, "grandchild ran\n", 15);
        return 0;
    }
    int p[2];
    if (pipe(p)) return 1;
    char fd[16];
    snprintf(fd, sizeof fd, "%d", p[1]);
    static char stack[64 * 1024] __attribute__((aligned(16)));
    char *args[] = {argv[0], "grand", fd, NULL};
    pid_t c = clone(child, stack + sizeof stack, CLONE_VM | CLONE_VFORK | SIGCHLD, args);
    int st = 0;
    waitpid(c, &st, 0);
    printf("child: exited %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -WTERMSIG(st));
    close(p[1]);
    char buf[64];
    ssize_t n = read(p[0], buf, sizeof buf - 1);
    buf[n > 0 ? n : 0] = 0;
    printf("%s", n > 0 ? buf : "no word from the grandchild\n");
    return 0;
}
