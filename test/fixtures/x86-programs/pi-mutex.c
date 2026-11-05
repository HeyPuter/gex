/* Priority-inheritance mutexes (PTHREAD_PRIO_INHERIT): the libc takes them
 * with FUTEX_LOCK_PI / FUTEX_UNLOCK_PI / FUTEX_TRYLOCK_PI, which gex did not
 * have (ENOSYS), so a contended PI mutex never woke its waiter -- Unity's
 * player deadlocked at start-up. Four threads increment a counter under one
 * PI mutex; trylock reports a held lock as busy. */
#include <errno.h>
#include <pthread.h>
#include <stdio.h>

static pthread_mutex_t m;
static long counter;

static void *worker(void *arg) {
    (void)arg;
    for (int i = 0; i < 20000; i++) {
        pthread_mutex_lock(&m);
        counter++;
        pthread_mutex_unlock(&m);
    }
    return NULL;
}

static int busy_elsewhere;

static void *try_from_another(void *arg) {
    (void)arg;
    busy_elsewhere = pthread_mutex_trylock(&m) == EBUSY;
    return NULL;
}

int main(void) {
    pthread_mutexattr_t a;
    pthread_mutexattr_init(&a);
    if (pthread_mutexattr_setprotocol(&a, PTHREAD_PRIO_INHERIT) != 0) {
        printf("no PRIO_INHERIT\n");
        return 1;
    }
    pthread_mutex_init(&m, &a);
    pthread_t t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], NULL, worker, NULL);
    for (int i = 0; i < 4; i++) pthread_join(t[i], NULL);
    /* Held by this thread: another thread's trylock sees it busy. */
    pthread_mutex_lock(&m);
    pthread_t other;
    pthread_create(&other, NULL, try_from_another, NULL);
    pthread_join(other, NULL);
    pthread_mutex_unlock(&m);
    /* Free: a trylock takes it. */
    int free_ok = pthread_mutex_trylock(&m) == 0;
    pthread_mutex_unlock(&m);
    printf("counter %ld, held lock busy elsewhere: %s, free lock taken: %s\n", counter,
           busy_elsewhere ? "yes" : "no", free_ok ? "yes" : "no");
    return 0;
}
