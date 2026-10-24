/* The calls that wait for nothing but a signal: pause, sigsuspend, and
 * ppoll, poll, pselect and select with no descriptors and no timeout. Each
 * must block until a signal arrives, run its handler, and fail with EINTR.
 * A second thread sends SIGUSR1 to the waiting one after 200 ms; the wait
 * must have lasted about that long. The mask-taking calls wait with SIGUSR1
 * blocked outside them, so the signal is let in only by the call's mask;
 * without a mask (pause's own form where there is no pause call), they wait
 * with SIGUSR1 open. */
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/select.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t handled;
static pthread_t waiter;
static int failures;

static void on_usr1(int s) { (void)s; handled = 1; }

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static void *signaller(void *p) {
    (void)p;
    struct timespec d = {0, 200 * 1000 * 1000};
    nanosleep(&d, NULL);
    pthread_kill(waiter, SIGUSR1);
    return NULL;
}

enum { PAUSE, SIGSUSPEND, PPOLL, PPOLL_NOMASK, POLL, PSELECT, PSELECT_NOMASK, SELECT };
static const char *names[] = {"pause", "sigsuspend", "ppoll", "ppoll without a mask", "poll", "pselect", "pselect without a mask", "select"};

static void check(int which) {
    sigset_t usr1, open;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    sigemptyset(&open);
    int masked = which == SIGSUSPEND || which == PPOLL || which == PSELECT;
    pthread_sigmask(masked ? SIG_BLOCK : SIG_UNBLOCK, &usr1, NULL);
    handled = 0;
    pthread_t t;
    pthread_create(&t, NULL, signaller, NULL);
    double t0 = now();
    int r = -1;
    switch (which) {
    case PAUSE: r = pause(); break;
    case SIGSUSPEND: r = sigsuspend(&open); break;
    case PPOLL: r = ppoll(NULL, 0, NULL, &open); break;
    case PPOLL_NOMASK: r = ppoll(NULL, 0, NULL, NULL); break;
    case POLL: r = poll(NULL, 0, -1); break;
    case PSELECT: r = pselect(0, NULL, NULL, NULL, NULL, &open); break;
    case PSELECT_NOMASK: r = pselect(0, NULL, NULL, NULL, NULL, NULL); break;
    case SELECT: r = select(0, NULL, NULL, NULL, NULL); break;
    }
    int e = errno;
    double slept = now() - t0;
    /* Read before anything else can take the signal: a call that returned
     * early takes it in the join or the unmask. */
    int ran = handled;
    pthread_join(t, NULL);
    pthread_sigmask(SIG_UNBLOCK, &usr1, NULL);
    if (r == -1 && e == EINTR && ran && slept >= 0.15 && slept < 20) {
        printf("ok   %s\n", names[which]);
    } else {
        printf("FAIL %s\n", names[which]);
        fprintf(stderr, "%s: returned %d (%s) after %.3f s, handler %s\n", names[which], r,
                r == -1 ? strerror(e) : "no error", slept, ran ? "ran" : "did not run");
        failures++;
    }
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, NULL);
    waiter = pthread_self();
    for (int i = PAUSE; i <= SELECT; i++) check(i);
    return failures;
}
