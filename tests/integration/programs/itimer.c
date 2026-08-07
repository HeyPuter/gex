/* Interval timers, which are how a program puts a bound on its own work.
 *
 * `setitimer` used to be accepted and dropped: it returned success and the
 * signal never came. That is worse than refusing, because a program that
 * cannot arm a timeout knows it has none, while a program whose timeout never
 * fires waits forever. `openssl speed` runs its measurement loop until SIGALRM
 * arrives and has no other stopping condition, so a two-second benchmark
 * became an infinite one.
 *
 * The counts here are deliberately not compared -- how many times a repeating
 * timer fires in a fixed wall-clock window is not reproducible under an
 * emulator -- only that it fired at all, that it stopped when disarmed, and
 * that what `getitimer` reports is inside the interval it was given.
 */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

static volatile sig_atomic_t alarms;

static void on_alarm(int signo) {
    (void)signo;
    alarms++;
}

/* Something the compiler cannot lift out of the loop, so the wait is real
   work rather than a spin the optimiser deletes. */
static volatile unsigned long sink;

static void burn(void) {
    unsigned long h = 1;
    for (int i = 0; i < 200000; i++) {
        h = h * 6364136223846793005UL + 1442695040888963407UL;
        sink = h;
    }
}

static void arm(long sec, long usec, long isec, long iusec) {
    struct itimerval t;
    t.it_value.tv_sec = sec;
    t.it_value.tv_usec = usec;
    t.it_interval.tv_sec = isec;
    t.it_interval.tv_usec = iusec;
    if (setitimer(ITIMER_REAL, &t, NULL) != 0) {
        perror("setitimer");
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_alarm;
    sigaction(SIGALRM, &sa, NULL);

    /* A one-shot timer must fire exactly once, however long we wait. */
    alarms = 0;
    arm(0, 50000, 0, 0);
    while (alarms == 0) {
        burn();
    }
    for (int i = 0; i < 20; i++) {
        burn();
    }
    printf("one-shot fired: %s\n", alarms == 1 ? "once" : "more than once");

    /* getitimer reports what is left, which must be inside what was asked
       for and must be zero once the timer has gone off. */
    struct itimerval got;
    arm(10, 0, 0, 0);
    getitimer(ITIMER_REAL, &got);
    printf("remaining in range: %s\n",
           (got.it_value.tv_sec <= 10 && got.it_value.tv_sec >= 8) ? "yes" : "no");
    printf("interval reported: %ld.%06ld\n", (long)got.it_interval.tv_sec,
           (long)got.it_interval.tv_usec);

    /* Disarming must stop it: nothing arrives after this, however long the
       program then runs. */
    alarms = 0;
    arm(0, 0, 0, 0);
    getitimer(ITIMER_REAL, &got);
    printf("disarmed: %s\n",
           (got.it_value.tv_sec == 0 && got.it_value.tv_usec == 0) ? "yes" : "no");
    for (int i = 0; i < 200; i++) {
        burn();
    }
    printf("after disarm: %d\n", (int)alarms);

    /* A repeating timer fires more than once and keeps going. */
    alarms = 0;
    arm(0, 20000, 0, 20000);
    while (alarms < 3) {
        burn();
    }
    arm(0, 0, 0, 0);
    printf("repeating fired at least three times: yes\n");

    /* An out-of-range `which` is EINVAL, not a crash. */
    errno = 0;
    struct itimerval junk;
    memset(&junk, 0, sizeof junk);
    int rc = setitimer(99, &junk, NULL);
    printf("bad which: rc=%d errno=%s\n", rc, rc < 0 && errno == EINVAL ? "EINVAL" : "other");
    return 0;
}
