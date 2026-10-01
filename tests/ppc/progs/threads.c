/* Threads on the PowerPC guest: clone, TLS through r2, futex waits and wakes
 * (pthread_join, mutexes, condition variables), and C11 atomics (lwarx/stwcx.). */
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
enum { N = 4, ITERS = 20000 };
static _Thread_local int tls_id;
static atomic_int counter;
static int locked_counter;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int ready;
static void *worker(void *arg) {
    tls_id = (int)(long)arg;
    pthread_mutex_lock(&mu);
    while (!ready) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    for (int i = 0; i < ITERS; ++i) {
        atomic_fetch_add(&counter, 1);
        pthread_mutex_lock(&mu); locked_counter += tls_id; pthread_mutex_unlock(&mu);
    }
    return (void *)(long)(tls_id * 10);
}
int main(void) {
    pthread_t t[N];
    tls_id = 99;
    for (long i = 0; i < N; ++i) pthread_create(&t[i], 0, worker, (void *)(i + 1));
    pthread_mutex_lock(&mu); ready = 1; pthread_cond_broadcast(&cv); pthread_mutex_unlock(&mu);
    long sum = 0;
    for (int i = 0; i < N; ++i) { void *r; pthread_join(t[i], &r); sum += (long)r; }
    int expected = 0; atomic_compare_exchange_strong(&counter, &expected, -1);
    printf("counter=%d locked=%d joined=%ld main_tls=%d cas_failed=%d\n", atomic_load(&counter), locked_counter, sum, tls_id, expected != 0);
    return 0;
}
