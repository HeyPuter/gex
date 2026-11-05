/* The small descriptor and process calls from several threads at once
 * (epoll_ctl, pipe2, ftruncate, fstatfs, getpriority/setpriority,
 * sysinfo and prlimit64 without the kernel lock).
 *
 * - Two threads make pipes, pass a byte through each and close them.
 * - Two threads add their pipe's read end to one shared epoll set, write
 *   to it and must see it ready through the set (waited on by the thread
 *   itself), then delete it; a second add of a watched one is EEXIST.
 * - One thread truncates a shared file to sizes it then reads back with
 *   fstat while another grows and shrinks a file of its own.
 * - One thread sets RLIMIT_NOFILE's soft limit and reads it back while the
 *   pipe threads open descriptors; the limit stays what was last set.
 * - statfs, fstatfs, sysinfo and the priority calls answer throughout. */
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/sysinfo.h>
#include <fcntl.h>
#include <unistd.h>

#define ROUNDS 300

static int ep;
static atomic_int bad;

static void fail(const char *what, int n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%d, errno %d)\n", what, n, errno);
}

static void *piper(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        int p[2];
        if (pipe2(p, O_CLOEXEC) != 0) { fail("pipe2", i); continue; }
        char c = (char)i, d = 0;
        if (write(p[1], &c, 1) != 1 || read(p[0], &d, 1) != 1 || d != c) fail("pipe bytes", i);
        close(p[0]);
        close(p[1]);
    }
    return 0;
}

static void *watcher(void *arg) {
    int id = (int)(long)arg;
    for (int i = 0; i < ROUNDS; i++) {
        int p[2];
        if (pipe2(p, O_NONBLOCK) != 0) { fail("pipe2 (watcher)", i); continue; }
        struct epoll_event ev = { .events = EPOLLIN, .data.u64 = ((unsigned long long)id << 32) | (unsigned)i };
        if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev) != 0) fail("epoll_ctl add", i);
        if (epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev) == 0 || errno != EEXIST) fail("second add not EEXIST", i);
        char c = 'x';
        if (write(p[1], &c, 1) != 1) fail("write", i);
        /* Its own event among whatever the other watcher's are. */
        int seen = 0;
        for (int tries = 0; tries < 100 && !seen; tries++) {
            struct epoll_event got[8];
            int n = epoll_wait(ep, got, 8, 100);
            for (int j = 0; j < n; j++)
                if (got[j].data.u64 == ev.data.u64) seen = 1;
        }
        if (!seen) fail("never ready through the set", i);
        if (epoll_ctl(ep, EPOLL_CTL_DEL, p[0], 0) != 0) fail("epoll_ctl del", i);
        if (epoll_ctl(ep, EPOLL_CTL_MOD, p[0], &ev) == 0 || errno != ENOENT) fail("mod after del not ENOENT", i);
        close(p[0]);
        close(p[1]);
    }
    return 0;
}

static void *truncater(void *arg) {
    const char *name = arg;
    int fd = open(name, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { fail("open", 0); return 0; }
    for (int i = 0; i < ROUNDS; i++) {
        off_t want = (off_t)((i * 7919) % 65536);
        if (ftruncate(fd, want) != 0) fail("ftruncate", i);
        struct stat st;
        if (fstat(fd, &st) != 0 || st.st_size != want) fail("size after ftruncate", i);
        struct statfs fs;
        if (fstatfs(fd, &fs) != 0 || fs.f_bsize == 0) fail("fstatfs", i);
    }
    close(fd);
    unlink(name);
    return 0;
}

static void *limiter(void *arg) {
    (void)arg;
    struct rlimit hard;
    getrlimit(RLIMIT_NOFILE, &hard);
    for (int i = 0; i < ROUNDS; i++) {
        struct rlimit r = { .rlim_cur = 256 + (rlim_t)(i % 64), .rlim_max = hard.rlim_max };
        if (setrlimit(RLIMIT_NOFILE, &r) != 0) fail("setrlimit", i);
        struct rlimit back;
        if (getrlimit(RLIMIT_NOFILE, &back) != 0 || back.rlim_cur != r.rlim_cur) fail("getrlimit back", i);
        struct sysinfo si;
        if (sysinfo(&si) != 0 || si.totalram == 0) fail("sysinfo", i);
        struct statfs fs;
        if (statfs("/", &fs) != 0 || fs.f_bsize == 0) fail("statfs", i);
        errno = 0;
        if (getpriority(PRIO_PROCESS, 0) == -1 && errno) fail("getpriority", i);
    }
    return 0;
}

int main(void) {
    /* Its own directory: concurrent runs under gexc share /tmp. */
    static char dir[64] = "/tmp/fdmisc-XXXXXX", fa[80], fb[80];
    if (!mkdtemp(dir)) return 2;
    snprintf(fa, sizeof fa, "%s/a", dir);
    snprintf(fb, sizeof fb, "%s/b", dir);
    ep = epoll_create1(EPOLL_CLOEXEC);
    if (ep < 0) return 2;
    pthread_t t[7];
    pthread_create(&t[0], 0, piper, 0);
    pthread_create(&t[1], 0, piper, 0);
    pthread_create(&t[2], 0, watcher, (void *)1L);
    pthread_create(&t[3], 0, watcher, (void *)2L);
    pthread_create(&t[4], 0, truncater, fa);
    pthread_create(&t[5], 0, truncater, fb);
    pthread_create(&t[6], 0, limiter, 0);
    for (int i = 0; i < 7; i++) pthread_join(t[i], 0);
    rmdir(dir);
    struct rlimit r;
    getrlimit(RLIMIT_NOFILE, &r);
    printf("rounds %d bad %d nofile %lu\n", ROUNDS, atomic_load(&bad), (unsigned long)r.rlim_cur);
    return atomic_load(&bad) != 0;
}
