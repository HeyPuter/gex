// A watch belongs to the open file it was added for, not to the number: a
// descriptor closed without EPOLL_CTL_DEL leaves its set once that file is
// gone, and a new file that is given the same number is not reported under
// the old watch's token, and can be added afresh.
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>

int main(void) {
    int ep = epoll_create1(0);
    int a = eventfd(0, EFD_NONBLOCK);
    struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 1};
    epoll_ctl(ep, EPOLL_CTL_ADD, a, &ev);
    close(a);
    int b = eventfd(1, EFD_NONBLOCK); // readable, and given a's number
    printf("same number %d\n", a == b);
    struct epoll_event out[4];
    int n = epoll_wait(ep, out, 4, 0);
    printf("stale reports %d", n);
    for (int i = 0; i < n; i++)
        printf(" token %llu", (unsigned long long)out[i].data.u64);
    printf("\n");
    ev.data.u64 = 2;
    int r = epoll_ctl(ep, EPOLL_CTL_ADD, b, &ev);
    printf("add again %d %s\n", r, r ? (errno == EEXIST ? "EEXIST" : "error") : "ok");
    n = epoll_wait(ep, out, 4, 0);
    printf("reports %d", n);
    for (int i = 0; i < n; i++)
        printf(" token %llu", (unsigned long long)out[i].data.u64);
    printf("\n");
    // A watch on a file still open under another number stays, as Linux's does.
    int c = eventfd(1, EFD_NONBLOCK);
    ev.data.u64 = 3;
    epoll_ctl(ep, EPOLL_CTL_ADD, c, &ev);
    int c2 = dup(c);
    close(c);
    n = epoll_wait(ep, out, 4, 0);
    int seen3 = 0;
    for (int i = 0; i < n; i++)
        seen3 += out[i].data.u64 == 3;
    printf("dup keeps watch %d\n", seen3);
    close(c2);
    return 0;
}
