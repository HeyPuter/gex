/* execve: argv, envp, close-on-exec, and a failed exec. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/wait.h>

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child")) {
        printf("argc=%d", argc);
        for (int i = 1; i < argc; i++) printf(" [%s]", argv[i]);
        printf("\n");
        printf("env GEXA=%s GEXB=%s\n", getenv("GEXA") ?: "(unset)", getenv("GEXB") ?: "(unset)");
        /* fd 7 was marked close-on-exec; fd 8 was not. */
        printf("fd7 %s  fd8 %s\n",
               fcntl(7, F_GETFD) < 0 ? "closed" : "open",
               fcntl(8, F_GETFD) < 0 ? "closed" : "open");
        char buf[32] = {0};
        ssize_t n = read(8, buf, sizeof buf - 1);
        printf("inherited fd reads %zd bytes: %s\n", n, n > 0 ? buf : "");
        return 42;
    }

    FILE *f = fopen("/tmp/gex-exec-data", "w");
    fputs("through the pipe", f);
    fclose(f);

    int a = open("/tmp/gex-exec-data", O_RDONLY | O_CLOEXEC);
    int b = open("/tmp/gex-exec-data", O_RDONLY);
    dup2(a, 7); dup2(b, 8);
    fcntl(7, F_SETFD, FD_CLOEXEC);

    pid_t p = fork();
    if (p == 0) {
        /* argv[0], so the test does not depend on the working directory. */
        char *args[] = {argv[0], "child", "two", "three", NULL};
        char *env[] = {"GEXA=one", "GEXB=two", NULL};
        execve(argv[0], args, env);
        _exit(127);
    }
    int st = 0; waitpid(p, &st, 0);
    printf("child status %d\n", WEXITSTATUS(st));

    /* A failed exec must return, not replace the process. */
    p = fork();
    if (p == 0) {
        char *args[] = {"/nonexistent", NULL};
        execve("/nonexistent", args, NULL);
        _exit(99);
    }
    waitpid(p, &st, 0);
    printf("failed exec status %d\n", WEXITSTATUS(st));
    printf("done\n");
    return 0;
}
