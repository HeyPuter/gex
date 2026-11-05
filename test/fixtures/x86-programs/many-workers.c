/* More guest threads alive at once, across processes, than the global
 * kernel once had stacks for (128: "the global kernel has 128 stacks and
 * this is Worker 129", the Win7 shell's Games folder). In the page every
 * guest thread is a Worker with a kernel stack slice of its own: three
 * children of 48 threads each, all parked at once, then released. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHILDREN 3
#define THREADS 48

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int started, released;

static void *held(void *arg)
{
    pthread_mutex_lock(&mu);
    started++;
    pthread_cond_broadcast(&cv);
    while (!released) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    return arg;
}

static int child(int ready, int go)
{
    pthread_t t[THREADS];
    for (long i = 0; i < THREADS; i++)
        if (pthread_create(&t[i], 0, held, (void *)i)) return 2;
    pthread_mutex_lock(&mu);
    while (started < THREADS) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    char c = 'r';
    if (write(ready, &c, 1) != 1) return 3;
    if (read(go, &c, 1) != 1) return 4;
    pthread_mutex_lock(&mu);
    released = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
    long sum = 0;
    for (int i = 0; i < THREADS; i++) {
        void *r;
        pthread_join(t[i], &r);
        sum += (long)r;
    }
    return sum == (long)THREADS * (THREADS - 1) / 2 ? 0 : 5;
}

int main(void)
{
    int ready[2], go[2];
    if (pipe(ready) || pipe(go)) return 1;
    pid_t kids[CHILDREN];
    for (int k = 0; k < CHILDREN; k++) {
        kids[k] = fork();
        if (kids[k] == 0) _exit(child(ready[1], go[0]));
    }
    int n = 0;
    char c;
    while (n < CHILDREN && read(ready[0], &c, 1) == 1) n++;
    printf("%d children with %d threads each parked at once\n", n, THREADS);
    fflush(stdout);
    for (int k = 0; k < CHILDREN; k++)
        if (write(go[1], "g", 1) != 1) return 1;
    int ok = 0;
    for (int k = 0; k < CHILDREN; k++) {
        int st;
        if (waitpid(kids[k], &st, 0) == kids[k] && WIFEXITED(st) && WEXITSTATUS(st) == 0) ok++;
        else printf("child %d: status %#x\n", k, st);
    }
    printf("%d of %d children ended cleanly\n", ok, CHILDREN);
    return ok == CHILDREN ? 0 : 1;
}
