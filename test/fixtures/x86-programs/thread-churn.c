/* More threads over a process's life than the page has Worker slots (64):
 * 100 threads created and joined one after another, each using a
 * thread-local; then, with 40 threads alive, a fork whose child runs 30
 * threads at once. On the page every guest thread is a shim instance with a
 * stack and thread-local slot; slots that were never given back left the
 * 65th instance with no thread-locals (__tls_base zero, over guest memory),
 * and a forked child that kept its parent's slot bits had too few left. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static __thread int mine;

static void *one(void *arg)
{
    mine = (int)(long)arg;
    getpid();
    return (void *)(long)(mine * 2);
}

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int released;

static void *held(void *arg)
{
    mine = (int)(long)arg;
    pthread_mutex_lock(&mu);
    while (!released) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    return (void *)(long)mine;
}

static pthread_barrier_t bar;

static void *together(void *arg)
{
    mine = (int)(long)arg;
    pthread_barrier_wait(&bar);
    return (void *)(long)(mine + 1);
}

int main(void)
{
    long sum = 0;
    for (long i = 0; i < 100; i++) {
        pthread_t t;
        void *r;
        if (pthread_create(&t, NULL, one, (void *)i)) { printf("create %ld failed\n", i); return 1; }
        pthread_join(t, &r);
        sum += (long)r;
    }
    printf("sequential: 100 threads, sum %ld\n", sum);

    pthread_t h[40];
    for (long i = 0; i < 40; i++)
        if (pthread_create(&h[i], NULL, held, (void *)i)) { printf("held %ld failed\n", i); return 1; }
    pid_t pid = fork();
    if (pid == 0) {
        pthread_t t[30];
        long s = 0;
        pthread_barrier_init(&bar, NULL, 30);
        for (long i = 0; i < 30; i++)
            if (pthread_create(&t[i], NULL, together, (void *)i)) _exit(2);
        for (int i = 0; i < 30; i++) { void *r; pthread_join(t[i], &r); s += (long)r; }
        _exit(s == 465 ? 0 : 3);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    printf("fork child with 30 threads: %s\n", WIFEXITED(st) && WEXITSTATUS(st) == 0 ? "ok" : "failed");
    pthread_mutex_lock(&mu);
    released = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
    long hs = 0;
    for (int i = 0; i < 40; i++) { void *r; pthread_join(h[i], &r); hs += (long)r; }
    printf("held: 40 threads, sum %ld\n", hs);
    return 0;
}
