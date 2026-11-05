/* A vfork child that spawns (Qt's QProcess::startDetached: vfork, and in the
   child vforkfd, a CLONE_VM | CLONE_VFORK clone, then exec): the grandchild
   runs and the parent hears from it. musl's vfork is a fork on AArch64, so
   the child is cloned as glibc's vfork clones it.

   The grandchild's exec may fail, and its parent must hear why: from a pipe
   (musl's posix_spawn, Qt's startDetached), and from the memory the two
   share, which the grandchild writes before it exits (glibc's posix_spawn:
   `args->err`). And a vfork child may fork. */
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

struct job {
    const char *path; /* what the grandchild runs */
    char **args;
    volatile int err; /* the grandchild's exec error, as glibc's spawni keeps it */
};

static int spawned(void *a) {
    struct job *j = a;
    pid_t g;
    int r = posix_spawn(&g, j->path, NULL, NULL, j->args, environ);
    _exit(r == 0 ? 0 : 100 + r);
}

/* a vfork child may fork too: its child is a copy of the memory */
static int forked(void *a) {
    struct job *j = a;
    pid_t g = fork();
    if (g == 0) {
        execve(j->path, j->args, environ);
        _exit(127);
    }
    _exit(g > 0 ? 0 : 99);
}

static int grand(void *a) {
    struct job *j = a;
    execve(j->path, j->args, environ);
    j->err = errno;
    _exit(127);
}

static int shared(void *a) {
    struct job *j = a;
    static char stack[16 * 1024] __attribute__((aligned(16)));
    j->err = 0;
    pid_t g = clone(grand, stack + sizeof stack, CLONE_VM | CLONE_VFORK | SIGCHLD, j);
    if (g < 0) _exit(99);
    /* back only once the grandchild has exec'd or exited */
    _exit(j->err);
}

static int run(const char *name, int (*fn)(void *), struct job *j, int rd) {
    static char stack[64 * 1024] __attribute__((aligned(16)));
    pid_t c = clone(fn, stack + sizeof stack, CLONE_VM | CLONE_VFORK | SIGCHLD, j);
    int st = 0;
    waitpid(c, &st, 0);
    int code = WIFEXITED(st) ? WEXITSTATUS(st) : -WTERMSIG(st);
    printf("%s: exited %d\n", name, code);
    if (code == 0) {
        char buf[64];
        ssize_t n = read(rd, buf, sizeof buf - 1);
        buf[n > 0 ? n : 0] = 0;
        printf("%s", n > 0 ? buf : "no word from the grandchild\n");
    }
    fflush(stdout);
    return code;
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
    char *args[] = {argv[0], "grand", fd, NULL};
    /* by /proc/self/exe: argv[0] is a host path natively */
    struct job ok = {"/proc/self/exe", args, 0}, missing = {"/no/such/program", args, 0};
    run("spawn", spawned, &ok, p[0]);
    run("spawn missing", spawned, &missing, p[0]);
    /* argv[1]: "pipe" leaves out the shared-memory cases, "nofork" the fork */
    const char *only = argc > 1 ? argv[1] : "";
    if (!strstr(only, "nofork")) run("fork", forked, &ok, p[0]);
    if (strstr(only, "pipe")) return 0;
    run("shared", shared, &ok, p[0]);
    run("shared missing", shared, &missing, p[0]);
    return 0;
}
