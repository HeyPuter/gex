/* Thread ids in a forked child: a clone's tid never equals the child's main
 * thread's (its pid) or another live thread's. gex numbers threads per
 * process, and a child used to inherit its parent's counter, so the child's
 * Nth thread got tid == pid: two threads with one id, which pthread mutexes
 * (owner by tid) and tgkill confuse. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define N 12
static pid_t tids[N];
static pthread_barrier_t all_up;

static void *body(void *p)
{
    tids[(long)p] = (pid_t)syscall(SYS_gettid);
    pthread_barrier_wait(&all_up);   /* all live at once */
    return 0;
}

static int child(void)
{
    pthread_t t[N];
    pid_t me = getpid();
    int bad = 0;
    pthread_barrier_init(&all_up, 0, N);
    for (long i = 0; i < N; i++) pthread_create(&t[i], 0, body, (void *)i);
    for (int i = 0; i < N; i++) pthread_join(t[i], 0);
    for (int i = 0; i < N; i++) {
        if (tids[i] == me) bad++;
        for (int j = 0; j < i; j++) if (tids[i] == tids[j]) bad++;
    }
    printf("child: %d threads, %d duplicate ids\n", N, bad);
    fflush(stdout);
    return bad != 0;
}

int main(void)
{
    int fails = 0;
    for (int k = 0; k < 3; k++) {
        int st;
        pid_t p = fork();
        if (p == 0) _exit(child());
        waitpid(p, &st, 0);
        fails += !WIFEXITED(st) || WEXITSTATUS(st);
    }
    printf("%d of 3 children failed\n", fails);
    return fails != 0;
}
