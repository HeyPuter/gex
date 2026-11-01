/* mem64-miss: the guest of test/core/mem64-missing.test.js. Each way an
 * exec asks for a 64-bit memory (GEX_MEM64=1) -- a fork child's execve,
 * posix_spawn, and this process's own execve -- on a page whose memory64
 * runtime is missing: each fails with the errno it gives. */
#include <errno.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
#define ME "/g/miss"

int main(int argc, char **argv) {
    static char *av[] = { ME, "child", 0 };
    static char *env[] = { "GEX_MEM64=1", "HOME=/", 0 };
    int st = 0;
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc > 1) {
        printf("ran\n");
        return 0;
    }
    pid_t p = fork();
    if (p == 0) {
        execve(ME, av, env);
        printf("forked execve: %s\n", strerror(errno));
        _exit(3);
    }
    waitpid(p, &st, 0);
    printf("its exit: %d\n", WEXITSTATUS(st));
    pid_t q;
    int r = posix_spawn(&q, ME, 0, 0, av, env);
    printf("posix_spawn: %s\n", r ? strerror(r) : "ok");
    if (!r) waitpid(q, &st, 0);
    execve(ME, av, env);
    printf("execve: %s\n", strerror(errno));
    return 0;
}
