/* ELF providers across fork (docs/elf-providers.md): the child inherits the
 * parent's GOT, which holds the provided functions' addresses (an IFUNC's
 * resolver plus one), so the child must have the same bindings; and across
 * a fork plus exec of a program that maps libc again. */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static void work(const char *who) {
    char a[200], b[200];
    memset(a, 'x', sizeof a);
    a[150] = 0;
    memcpy(b, a, sizeof a);
    printf("%s: strlen %zu cmp %d chr %ld\n", who, strlen(b), memcmp(a, b, sizeof a) != 0, (long)(strchr(b, 0) - b));
    fflush(stdout);
}

int main(int argc, char **argv) {
    if (argc > 1) {
        work("exec'd");
        return 0;
    }
    work("parent");
    pid_t p = fork();
    if (p == 0) {
        work("child");
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    printf("child exit %d\n", WEXITSTATUS(st));
    fflush(stdout);
    p = fork();
    if (p == 0) {
        execl(argv[0], argv[0], "again", (char *)0);
        _exit(127);
    }
    waitpid(p, &st, 0);
    printf("exec child exit %d\n", WEXITSTATUS(st));
    return 0;
}
