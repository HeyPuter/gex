/* fork, exit status, and wait4. */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
#include <string.h>

static long shared_before_fork = 11;

int main(void) {
    /* A child sees the parent's memory as it was, and its writes stay its own. */
    static char buf[64];
    strcpy(buf, "set by parent");
    shared_before_fork = 42;

    pid_t p = fork();
    if (p < 0) { printf("fork failed\n"); return 1; }
    if (p == 0) {
        printf("child sees %ld and '%s'\n", shared_before_fork, buf);
        shared_before_fork = 99;
        strcpy(buf, "set by child");
        printf("child now %ld\n", shared_before_fork);
        fflush(stdout);
        _exit(3);
    }
    int st = 0;
    pid_t w = waitpid(p, &st, 0);
    printf("waited ok=%d exited=%d status=%d\n", w == p, WIFEXITED(st), WEXITSTATUS(st));
    printf("parent still %ld and '%s'\n", shared_before_fork, buf);

    /* Several children, collected in whatever order they finish. */
    int total = 0;
    for (int i = 0; i < 4; i++) {
        pid_t c = fork();
        if (c == 0) _exit(i + 1);
    }
    for (int i = 0; i < 4; i++) {
        int s2 = 0;
        pid_t c = wait(&s2);
        if (c > 0 && WIFEXITED(s2)) total += WEXITSTATUS(s2);
    }
    printf("collected total %d\n", total);

    /* No children left. */
    int s3 = 0;
    printf("wait with no children: %d\n", wait(&s3));
    printf("done\n");
    return 0;
}
