/* Does ITIMER_REAL (alarm()) fire, both when the process is spinning and when
 * it is asleep in nanosleep, which the signal must cut short? */
#include <signal.h>
#include <stdio.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

static volatile sig_atomic_t rang;
static void on_alarm(int s) { (void)s; rang = 1; }

int main(void) {
    signal(SIGALRM, on_alarm);

    /* 1. Spinning: the process is on the processor when the timer expires. */
    rang = 0;
    alarm(1);
    time_t t0 = time(NULL);
    while (!rang && time(NULL) - t0 < 4) { /* spin */ }
    printf("spinning: rang=%d after %lds\n", rang, (long)(time(NULL) - t0));
    alarm(0);

    /* 2. Asleep in nanosleep, which a signal must cut short. */
    rang = 0;
    alarm(1);
    t0 = time(NULL);
    struct timespec ts = {5, 0};
    int r = nanosleep(&ts, NULL);
    printf("nanosleep: r=%d rang=%d after %lds\n", r, rang,
           (long)(time(NULL) - t0));
    alarm(0);
    return 0;
}
