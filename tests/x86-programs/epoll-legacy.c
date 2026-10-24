// epoll_create and epoll_wait, the calls before their *1 and p forms, made
// directly (musl makes the newer ones; glibc programs such as the X server
// make these): a pipe's read end waited on with a timeout, then written.
#define _GNU_SOURCE
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    int p[2];
    if (pipe(p) != 0) return 1;
    long ep = syscall(SYS_epoll_create, 4);
    printf("epoll_create %s\n", ep >= 0 ? "ok" : "failed");
    struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 0x1122334455667788ull};
    printf("epoll_ctl %ld\n", (long)syscall(SYS_epoll_ctl, ep, EPOLL_CTL_ADD, p[0], &ev));
    struct epoll_event out[2] = {0};
    printf("epoll_wait empty %ld\n", (long)syscall(SYS_epoll_wait, ep, out, 2, 10));
    printf("write %ld\n", (long)write(p[1], "x", 1));
    long n = syscall(SYS_epoll_wait, ep, out, 2, -1);
    printf("epoll_wait %ld events %#x data %#llx\n", n, out[0].events, (unsigned long long)out[0].data.u64);
    return 0;
}
