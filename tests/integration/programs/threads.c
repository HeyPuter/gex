/* Guest threads: clone, TLS, futex-based joins and locking.
 *
 * Every result printed is order-independent, because the point is that the
 * arithmetic is right, not that the scheduler is deterministic.
 */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define N 8
#define ITERS 20000

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static long guarded;
static __thread int tls_value;
static pthread_once_t once = PTHREAD_ONCE_INIT;
static int once_count;

static void once_fn(void) { once_count++; }

struct arg { int id; long local; int tls_seen; };

static void *worker(void *p) {
    struct arg *a = p;
    pthread_once(&once, once_fn);
    /* Thread-local storage: each thread must see only its own. */
    tls_value = a->id * 100;
    for (int i = 0; i < ITERS; i++) {
        a->local += i;
        pthread_mutex_lock(&lock);
        guarded++;
        pthread_mutex_unlock(&lock);
    }
    a->tls_seen = tls_value;
    return &a->local;
}

/* A condition variable, which is a different futex pattern from a mutex. */
static pthread_mutex_t cv_lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int ready, consumed;

static void *producer(void *unused) {
    (void)unused;
    for (int i = 0; i < 100; i++) {
        pthread_mutex_lock(&cv_lock);
        ready++;
        pthread_cond_signal(&cv);
        pthread_mutex_unlock(&cv_lock);
    }
    return NULL;
}

static void *consumer(void *unused) {
    (void)unused;
    for (int i = 0; i < 100; i++) {
        pthread_mutex_lock(&cv_lock);
        while (ready == 0) pthread_cond_wait(&cv, &cv_lock);
        ready--;
        consumed++;
        pthread_mutex_unlock(&cv_lock);
    }
    return NULL;
}

int main(void) {
    pthread_t t[N];
    struct arg args[N];

    for (int i = 0; i < N; i++) {
        args[i] = (struct arg){.id = i + 1, .local = 0, .tls_seen = -1};
        if (pthread_create(&t[i], NULL, worker, &args[i]) != 0) {
            printf("pthread_create failed\n");
            return 1;
        }
    }
    long total = 0;
    int tls_ok = 1;
    for (int i = 0; i < N; i++) {
        void *r;
        pthread_join(t[i], &r);
        if (r != &args[i].local) tls_ok = 0;
        total += args[i].local;
        if (args[i].tls_seen != (i + 1) * 100) tls_ok = 0;
    }
    /* Each worker sums 0..ITERS-1 and there are N of them. */
    printf("sum %ld expected %ld\n", total, (long)N * ITERS * (ITERS - 1) / 2);
    printf("guarded %ld expected %d\n", guarded, N * ITERS);
    printf("tls %s\n", tls_ok ? "ok" : "WRONG");
    printf("once %d\n", once_count);

    /* The main thread's own TLS slot must be untouched by the workers. */
    printf("main tls %d\n", tls_value);

    pthread_t p, c;
    pthread_create(&c, NULL, consumer, NULL);
    pthread_create(&p, NULL, producer, NULL);
    pthread_join(p, NULL);
    pthread_join(c, NULL);
    printf("condvar consumed %d\n", consumed);

    /* Detached threads, which exit without anyone joining them. */
    pthread_attr_t at;
    pthread_attr_init(&at);
    pthread_attr_setdetachstate(&at, PTHREAD_CREATE_DETACHED);
    for (int i = 0; i < 4; i++) {
        pthread_t d;
        struct arg *da = calloc(1, sizeof *da);
        da->id = 100 + i;
        pthread_create(&d, &at, worker, da);
    }
    usleep(200000);
    printf("detached ran, guarded now %s\n", guarded > (long)N * ITERS ? "grew" : "did not grow");
    printf("done\n");
    return 0;
}
