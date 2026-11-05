#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
#define SIZE (384u << 20)
int main(int argc, char **argv) {
    if (argc > 1) {
        printf("exec'd\n");
        fflush(stdout);
        sleep(30);
        return 0;
    }
    char *p = malloc(SIZE);
    if (!p) return 2;
    for (unsigned i = 0; i < SIZE; i += 4096) p[i] = (char)(i >> 12 | 1);
    printf("filled\n");
    fflush(stdout);
    sleep(4);
    pid_t c = fork();
    if (c == 0) {
        execl(getenv("EFC") ? getenv("EFC") : argv[0], argv[0], "child", (char *)0);
        _exit(3);
    }
    int st;
    waitpid(c, &st, 0);
    return WIFEXITED(st) && WEXITSTATUS(st) == 0 && p[4096] == 1 ? 0 : 4;
}
