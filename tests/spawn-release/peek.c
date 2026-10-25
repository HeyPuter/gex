// What a game's page leg does: a harness reads a counter from the memory of
// a process that is a fork child (its memory came in its fork record) with
// a thread of its own (which registers that memory with the page), while
// the process runs. The record is let go once the child's Worker has it;
// the harness's view of the memory must last as long as the process.
#include <pthread.h>
#include <stdio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

volatile unsigned counter;

static void *tick(void *arg) {
    (void)arg;
    struct timespec t = {0, 20 * 1000 * 1000};
    for (int i = 0; i < 200; i++) {
        counter++;
        nanosleep(&t, 0);
    }
    return 0;
}

int main(void) {
    pid_t pid = fork();
    if (pid == 0) {
        pthread_t th;
        pthread_create(&th, 0, tick, 0);
        pthread_join(th, 0);
        printf("child counted %u\n", counter);
        fflush(stdout);
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    printf("child %s\n", WIFEXITED(st) && WEXITSTATUS(st) == 0 ? "ok" : "failed");
    return 0;
}
