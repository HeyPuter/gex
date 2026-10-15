/* The calls that make or work a descriptor's object, from several threads at
 * once (L22b: eventfd2, epoll_create1, memfd_create, fallocate, the timerfd
 * calls, sendfile, splice and copy_file_range without the kernel lock).
 *
 * - Two threads ping-pong through a pair of eventfds, each blocking in read.
 * - One thread makes timerfds, arms them a few milliseconds ahead and reads
 *   the expiry, and makes epoll sets that watch an eventfd.
 * - One thread makes memfds, grows them with fallocate, maps them and checks
 *   the size and a byte written through the map.
 * - One thread splices from an empty pipe, which blocks until another
 *   thread writes it, then copies the bytes on with sendfile and
 *   copy_file_range and checks them. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 200

static atomic_int bad;
static int ping, pong;
static int feed[2];

static void fail(const char *what, long n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%ld, errno %d)\n", what, n, errno);
}

static void *pinger(void *arg) {
    (void)arg;
    for (uint64_t i = 1; i <= ROUNDS; i++) {
        if (write(ping, &i, 8) != 8) fail("ping write", (long)i);
        uint64_t v = 0;
        if (read(pong, &v, 8) != 8 || v != i * 2) fail("pong read", (long)v);
    }
    return 0;
}

static void *ponger(void *arg) {
    (void)arg;
    for (uint64_t i = 1; i <= ROUNDS; i++) {
        uint64_t v = 0;
        if (read(ping, &v, 8) != 8 || v != i) fail("ping read", (long)v);
        v *= 2;
        if (write(pong, &v, 8) != 8) fail("pong write", (long)i);
    }
    return 0;
}

static void *timers(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS / 10; i++) {
        int t = timerfd_create(CLOCK_MONOTONIC, TFD_CLOEXEC);
        if (t < 0) { fail("timerfd_create", i); continue; }
        struct itimerspec its = { .it_value = { 0, 2000000 } };
        if (timerfd_settime(t, 0, &its, 0)) fail("timerfd_settime", i);
        struct itimerspec cur;
        if (timerfd_gettime(t, &cur) || cur.it_value.tv_sec != 0) fail("timerfd_gettime", i);
        uint64_t n = 0;
        if (read(t, &n, 8) != 8 || n < 1) fail("timer expiry", (long)n);
        close(t);
        int ep = epoll_create1(EPOLL_CLOEXEC);
        int e = eventfd(1, EFD_NONBLOCK);
        struct epoll_event ev = { .events = EPOLLIN, .data.u64 = 42 };
        if (ep < 0 || e < 0 || epoll_ctl(ep, EPOLL_CTL_ADD, e, &ev)) fail("epoll set", i);
        struct epoll_event got;
        if (epoll_wait(ep, &got, 1, 1000) != 1 || got.data.u64 != 42) fail("epoll_wait", i);
        close(e);
        close(ep);
    }
    return 0;
}

static void *memfds(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS / 4; i++) {
        int m = memfd_create("fdobj", MFD_CLOEXEC);
        if (m < 0) { fail("memfd_create", i); continue; }
        off_t len = 4096 * (1 + i % 4);
        if (fallocate(m, 0, 0, len)) fail("fallocate", i);
        struct stat st;
        if (fstat(m, &st) || st.st_size != len) fail("memfd size", (long)st.st_size);
        unsigned char *p = mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, m, 0);
        if (p == MAP_FAILED) { fail("memfd mmap", i); close(m); continue; }
        p[len - 1] = (unsigned char)i;
        unsigned char c = 0;
        if (pread(m, &c, 1, len - 1) != 1 || c != (unsigned char)i) fail("memfd byte", c);
        munmap(p, len);
        close(m);
    }
    return 0;
}

static void *feeder(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS / 10; i++) {
        usleep(1000);
        char msg[16];
        snprintf(msg, sizeof msg, "chunk%04d", i);
        if (write(feed[1], msg, 9) != 9) fail("feed write", i);
    }
    return 0;
}

static void *splicer(void *arg) {
    (void)arg;
    char path[64];
    snprintf(path, sizeof path, "/tmp/fdobj-threads-%d", (int)getpid());
    int f = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    int g[2];
    if (f < 0 || pipe(g)) { fail("splice setup", 0); return 0; }
    for (int i = 0; i < ROUNDS / 10; i++) {
        /* Blocks until the feeder writes. */
        loff_t at = (loff_t)i * 9;
        ssize_t n = splice(feed[0], 0, f, &at, 9, 0);
        if (n != 9) { fail("splice", (long)n); continue; }
        off_t from = (off_t)i * 9;
        if (sendfile(g[1], f, &from, 9) != 9) fail("sendfile", i);
        char got[10] = { 0 };
        if (read(g[0], got, 9) != 9) fail("pipe read", i);
        char want[16];
        snprintf(want, sizeof want, "chunk%04d", i);
        if (memcmp(got, want, 9)) fail("bytes moved", i);
    }
    /* And the whole file copied after itself. */
    const loff_t len = (loff_t)(ROUNDS / 10) * 9;
    loff_t in = 0, out = len;
    ssize_t c = copy_file_range(f, &in, f, &out, len, 0);
    if (c != len || in != len || out != 2 * len) fail("copy_file_range", (long)c);
    struct stat st;
    if (fstat(f, &st) || st.st_size != 2 * (ROUNDS / 10) * 9) fail("copied size", (long)st.st_size);
    close(f);
    close(g[0]);
    close(g[1]);
    unlink(path);
    return 0;
}

int main(void) {
    ping = eventfd(0, EFD_CLOEXEC);
    pong = eventfd(0, EFD_CLOEXEC);
    if (ping < 0 || pong < 0 || pipe(feed)) return 2;
    pthread_t t[6];
    pthread_create(&t[0], 0, pinger, 0);
    pthread_create(&t[1], 0, ponger, 0);
    pthread_create(&t[2], 0, timers, 0);
    pthread_create(&t[3], 0, memfds, 0);
    pthread_create(&t[4], 0, splicer, 0);
    pthread_create(&t[5], 0, feeder, 0);
    for (int i = 0; i < 6; i++) pthread_join(t[i], 0);
    printf("rounds %d bad %d\n", ROUNDS, atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
