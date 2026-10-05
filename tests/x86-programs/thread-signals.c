/* Signal masks and thread-directed signals are per thread. Mono's GC stops
 * the world by pthread_kill(SIGPWR)-ing each thread and waiting for each
 * handler to post a semaphore, while Unity's own worker threads run with
 * every signal blocked. gex kept one mask and one pending set per process
 * and raised a tgkill on the *caller*, so the GC waited forever.
 *
 * A thread with all signals blocked must not stop another thread from taking
 * a signal; a signal sent to a thread runs its handler on that thread, even
 * one blocked in a condition-variable wait; and the blocked thread still has
 * the signal pending when it unblocks. */
#define _GNU_SOURCE
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static sem_t ack;
static volatile pthread_t handled_on[4];
static volatile int nhandled;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int go;

static void on_usr1(int s) {
    (void)s;
    handled_on[__atomic_fetch_add(&nhandled, 1, __ATOMIC_SEQ_CST)] = pthread_self();
    sem_post(&ack);
}

static void *blocker(void *arg) {
    (void)arg;
    sigset_t all;
    sigfillset(&all);
    pthread_sigmask(SIG_BLOCK, &all, NULL);
    pthread_mutex_lock(&mu);
    while (!go) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    /* The signal sent to this thread while it blocked is still pending. */
    sigset_t pend;
    sigpending(&pend);
    return (void *)(long)sigismember(&pend, SIGUSR1);
}

static void *waiter(void *arg) {
    (void)arg;
    pthread_mutex_lock(&mu);
    while (!go) pthread_cond_wait(&cv, &mu);
    pthread_mutex_unlock(&mu);
    return NULL;
}

static void *block_all(void *arg) {
    sigset_t all;
    sigfillset(&all);
    pthread_sigmask(SIG_BLOCK, &all, NULL);
    return arg;
}

int main(void) {
    sem_init(&ack, 0, 0);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sa.sa_flags = SA_RESTART;
    sigaction(SIGUSR1, &sa, NULL);
    /* The main thread's own mask survives another thread (one that blocks
       everything) running first: gex once handed the main thread the mask of
       whichever thread had last been in the kernel. */
    pthread_t first;
    pthread_create(&first, NULL, block_all, NULL);
    pthread_join(first, NULL);
    pthread_kill(pthread_self(), SIGUSR1);
    int early = nhandled;
    pthread_t b, w;
    pthread_create(&b, NULL, blocker, NULL);
    pthread_create(&w, NULL, waiter, NULL);
    usleep(100000); /* both are waiting on the condition variable now */
    pthread_kill(w, SIGUSR1);
    sem_wait(&ack);
    pthread_kill(b, SIGUSR1);
    usleep(100000);
    int while_blocked = nhandled;
    pthread_mutex_lock(&mu);
    go = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
    void *pending_in_blocker;
    pthread_join(b, &pending_in_blocker);
    pthread_join(w, NULL);
    printf("handled on the main thread after an all-blocking thread: %s\n", early == 1 ? "yes" : "no");
    printf("handled on the waiting thread: %s\n", nhandled >= 2 && pthread_equal(handled_on[1], w) ? "yes" : "no");
    printf("blocked thread took it while blocked: %s\n", while_blocked > 2 ? "yes" : "no");
    printf("still pending in the blocked thread: %s\n", pending_in_blocker ? "yes" : "no");
    /* And a signal sent to the main thread runs its handler here. */
    int before = nhandled;
    pthread_kill(pthread_self(), SIGUSR1);
    printf("handled on the main thread: %s\n", nhandled == before + 1 && pthread_equal(handled_on[before], pthread_self()) ? "yes" : "no");
    return 0;
}
