/* rt_sigpending: signals raised while blocked show up as pending, and
 * sigtimedwait then accepts them. Deterministic, so QEMU is the oracle. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    sigset_t block;
    sigemptyset(&block);
    sigaddset(&block, SIGUSR1);
    sigaddset(&block, SIGUSR2);
    sigprocmask(SIG_BLOCK, &block, NULL);

    /* Nothing raised yet: pending is empty. */
    sigset_t p;
    sigemptyset(&p);
    sigpending(&p);
    printf("before: usr1=%d usr2=%d\n", sigismember(&p, SIGUSR1), sigismember(&p, SIGUSR2));

    /* Raise one; it cannot be delivered (blocked), so it stays pending. */
    raise(SIGUSR1);
    sigemptyset(&p);
    sigpending(&p);
    printf("after raise usr1: usr1=%d usr2=%d\n", sigismember(&p, SIGUSR1),
           sigismember(&p, SIGUSR2));

    /* Raise the other too. */
    raise(SIGUSR2);
    sigemptyset(&p);
    sigpending(&p);
    printf("after raise usr2: usr1=%d usr2=%d\n", sigismember(&p, SIGUSR1),
           sigismember(&p, SIGUSR2));

    /* Accept one; it should leave pending, the other should remain. */
    struct timespec ts = {.tv_sec = 0, .tv_nsec = 0};
    int got = sigtimedwait(&block, NULL, &ts);
    sigemptyset(&p);
    sigpending(&p);
    printf("accepted %d, now usr1=%d usr2=%d\n", got, sigismember(&p, SIGUSR1),
           sigismember(&p, SIGUSR2));
    printf("sigpending ok\n");
    return 0;
}
