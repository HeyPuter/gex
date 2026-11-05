// A fork child that runs on for a while after its parent has translated
// code: while it runs, the page's main thread must not keep its fork
// record, which names every module the parent had.
#include <stdio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

__attribute__((noinline)) long work(long n) {
    long s = 0;
    for (long i = 0; i < n; i++)
        s += i % 5;
    return s;
}

int main(void) {
    long s = work(200000);
    pid_t pid = fork();
    if (pid == 0) {
        struct timespec t = {2, 0};
        nanosleep(&t, 0);
        printf("child %ld\n", work(1000));
        fflush(stdout);
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    printf("parent %ld, child %s\n", s, WIFEXITED(st) && WEXITSTATUS(st) == 0 ? "ok" : "failed");
    return 0;
}
