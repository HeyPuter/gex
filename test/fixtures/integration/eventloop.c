/* epoll over an eventfd and a timerfd -- the core an event loop is built on.
 *
 * Deterministic on purpose, so QEMU can be the oracle: a one-shot timer and a
 * fixed eventfd counter, with output that names *what* happened rather than
 * *when*. The timing (how long epoll_wait blocks) differs between engines; the
 * bytes printed do not.
 */
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/timerfd.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(void) {
    int ep = epoll_create1(0);
    int ef = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    int tf = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC);
    if (ep < 0 || ef < 0 || tf < 0) {
        perror("create");
        return 1;
    }

    /* A one-shot timer 30ms out, and an eventfd already carrying a count. */
    struct itimerspec its;
    memset(&its, 0, sizeof its);
    its.it_value.tv_nsec = 30 * 1000 * 1000;
    timerfd_settime(tf, 0, &its, NULL);
    uint64_t seed = 7;
    write(ef, &seed, 8);

    struct epoll_event add = {.events = EPOLLIN, .data.fd = ef};
    epoll_ctl(ep, EPOLL_CTL_ADD, ef, &add);
    add.data.fd = tf;
    epoll_ctl(ep, EPOLL_CTL_ADD, tf, &add);

    int saw_ef = 0, saw_tf = 0;
    struct epoll_event out[4];
    /* Loop until both fds have been seen ready and drained once. */
    while (!(saw_ef && saw_tf)) {
        int n = epoll_wait(ep, out, 4, 2000);
        if (n <= 0) {
            printf("timed out waiting (saw_ef=%d saw_tf=%d)\n", saw_ef, saw_tf);
            return 1;
        }
        for (int i = 0; i < n; i++) {
            uint64_t v = 0;
            if (out[i].data.fd == ef && !saw_ef) {
                if (read(ef, &v, 8) == 8) {
                    printf("eventfd delivered %llu\n", (unsigned long long)v);
                    saw_ef = 1;
                }
            } else if (out[i].data.fd == tf && !saw_tf) {
                if (read(tf, &v, 8) == 8) {
                    printf("timerfd fired, expirations %llu\n", (unsigned long long)v);
                    saw_tf = 1;
                }
            }
        }
    }
    /* Both drained: a further wait with no timer armed times out at once. */
    int n = epoll_wait(ep, out, 4, 0);
    printf("drained, epoll_wait ready=%d\n", n);
    printf("event loop ok\n");
    return 0;
}
