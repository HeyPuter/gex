/* output-order: the guest of test/page/output-order.test.js. */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 10;
    for (int i = 0; i < n; i++) {
        pid_t p = fork();
        if (p == 0) {
            printf("child %d\n", i);
            fflush(stdout);
            _exit(0);
        }
        int st;
        waitpid(p, &st, 0);
        printf("parent %d\n", i);
        fflush(stdout);
    }
    return 0;
}
