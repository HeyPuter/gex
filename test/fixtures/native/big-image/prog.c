/* A program run as one too big for a module is: it starts, calls through a
 * table, forks, and its child execs it again. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

static int add(int a, int b) { return a + b; }
static int mul(int a, int b) { return a * b; }
static int (*const ops[])(int, int) = { add, mul };

int main(int argc, char **argv)
{
    int v = 0;
    for (int i = 0; i < 100; i++)
        v = ops[i & 1](v, 3) % 1000;
    if (argc > 1 && !strcmp(argv[1], "child")) {
        printf("child %d\n", v);
        return 0;
    }
    printf("parent %d\n", v);
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        execl("/prog", "prog", "child", (char *)0);
        _exit(127);
    }
    int st;
    waitpid(p, &st, 0);
    printf("child exit %d\n", WEXITSTATUS(st));
    return 0;
}
