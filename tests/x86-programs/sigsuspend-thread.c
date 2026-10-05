/* rt_sigsuspend's "mask to put back" is the suspending thread's. gex kept it
 * in the shared kernel, so while one thread sat in sigsuspend the next thread
 * to finish *any* blocking call (here a futex wait in sem_wait) took that
 * mask as its own. Mono's suspend handler sigsuspends this way, and Muck's
 * main thread was left with SIGPWR blocked, hanging the next GC. */
#define _GNU_SOURCE
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>

static sem_t in_suspend, wake;

static void on_usr1(int s) { (void)s; }

static void *suspender(void *p) {
    (void)p;
    sigset_t all, wait;
    sigfillset(&all);
    pthread_sigmask(SIG_SETMASK, &all, NULL); /* the mask to put back */
    sigfillset(&wait);
    sigdelset(&wait, SIGUSR1);
    sem_post(&in_suspend);
    sigsuspend(&wait);
    return NULL;
}

int main(void) {
    signal(SIGUSR1, on_usr1);
    sem_init(&in_suspend, 0, 0);
    sem_init(&wake, 0, 0);
    pthread_t t;
    pthread_create(&t, NULL, suspender, NULL);
    sem_wait(&in_suspend);
    usleep(100 * 1000); /* let it reach sigsuspend */
    /* A blocking wait of our own, finished while the other is suspended. */
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_nsec += 50 * 1000 * 1000;
    if (ts.tv_nsec >= 1000000000) { ts.tv_sec++; ts.tv_nsec -= 1000000000; }
    sem_timedwait(&wake, &ts);
    sigset_t now;
    pthread_sigmask(SIG_SETMASK, NULL, &now);
    int bad = sigismember(&now, SIGTERM);
    printf("main mask after wait: %s\n", bad ? "took the suspender's" : "own");
    pthread_kill(t, SIGUSR1);
    pthread_join(t, NULL);
    puts(bad ? "FAIL" : "ok");
    return bad;
}
