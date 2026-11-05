// A forked child runs code its parent translated, where the parent moved
// the code's pages to another base since (as a page goes to the base a
// unit built in another process has there). The child's directory is the
// parent's at the fork, and adopting the parent's translations must not
// write their older bases back over it.
#include <pthread.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

__attribute__((noinline)) long work(long n) {
    long s = 0;
    for (long i = 0; i < n; i++)
        s += i % 7;
    return s;
}

static void *thread(void *arg) {
    (void)arg;
    return (void *)work(1000);
}

int main(void) {
    // A second thread: a process that has run one keeps its translations
    // for a child to adopt
    pthread_t t;
    void *r;
    pthread_create(&t, 0, thread, 0);
    pthread_join(t, &r);
    long s = work(100000);
    pid_t pid = fork();
    if (pid == 0) {
        printf("child %ld\n", work(100000));
        fflush(stdout);
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    printf("parent %ld %ld, child %s\n", s, (long)r, WIFEXITED(st) && WEXITSTATUS(st) == 0 ? "ok" : "failed");
    return 0;
}
