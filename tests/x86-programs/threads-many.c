/* More threads alive at once in one process than a Worker's shim once had
 * stacks for (64: "no shim stack slot left for this Worker"). A round
 * starts its threads and parks every one before any is released, so all
 * are live together; then they are released and joined, and each returns
 * its own number. First 100, which must all start. Then 300, past what the
 * page runs at once (256 Workers a process): there pthread_create must say
 * EAGAIN, and every thread it did start must run and be joined; natively
 * all 300 start. How many started goes to stderr, so the output is the same
 * either way. With the argument "first" (a host that cannot run 300
 * threads), the first round only. Prints "threads-many ok". */
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>

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

/* Start up to `want` threads at once; how many started, or -1. */
static int round_of(int want)
{
    static pthread_t t[300];
    pthread_attr_t at;
    pthread_attr_init(&at);
    pthread_attr_setstacksize(&at, 64 * 1024);
    started = released = 0;
    int made = 0;
    for (long i = 0; i < want; i++) {
        int e = pthread_create(&t[i], &at, held, (void *)i);
        if (e) {
            if (e != EAGAIN) {
                printf("pthread_create %ld: %s\n", i, strerror(e));
                return -1;
            }
            break;
        }
        made++;
    }
    pthread_mutex_lock(&mu);
    while (started < made) pthread_cond_wait(&cv, &mu);
    released = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
    long sum = 0;
    for (int i = 0; i < made; i++) {
        void *r;
        pthread_join(t[i], &r);
        sum += (long)r;
    }
    if (sum != (long)made * (made - 1) / 2) {
        printf("joined sum %ld, want %ld\n", sum, (long)made * (made - 1) / 2);
        return -1;
    }
    fprintf(stderr, "threads-many: %d of %d threads live at once\n", made, want);
    return made;
}

int main(int argc, char **argv)
{
    if (round_of(100) != 100) return 1;
    printf("100 threads live at once\n");
    if (argc > 1 && !strcmp(argv[1], "first")) {
        printf("threads-many ok\n");
        return 0;
    }
    int made = round_of(300);
    if (made < 200) return 2;
    printf("300 asked: each started thread ran, the rest refused with EAGAIN\n");
    printf("threads-many ok\n");
    return 0;
}
