/* ioctl from several threads at once, answered without the kernel lock: FIONREAD on a pipe another thread is
 * filling and draining, FIONBIO and FIOCLEX/FIONCLEX flipped on shared
 * descriptors, and terminal requests on a descriptor that is not one. The
 * counts and the final flags come out the same as on Linux. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <termios.h>
#include <unistd.h>

#define ROUNDS 4000
static int pfd[2];
static long seen_bad, notty, flips;

/* Writes one byte at a time; the reader takes them back. FIONREAD never
 * reports more than the pipe could hold or less than nothing. */
static void *writer(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        char c = (char)i;
        if (write(pfd[1], &c, 1) != 1) __atomic_add_fetch(&seen_bad, 1, __ATOMIC_RELAXED);
    }
    return NULL;
}

static void *reader(void *arg) {
    (void)arg;
    long got = 0;
    while (got < ROUNDS) {
        int n = -1;
        if (ioctl(pfd[0], FIONREAD, &n) != 0 || n < 0 || n > 65536) __atomic_add_fetch(&seen_bad, 1, __ATOMIC_RELAXED);
        char buf[256];
        if (n > 0) {
            ssize_t r = read(pfd[0], buf, n > (int)sizeof buf ? (int)sizeof buf : n);
            if (r > 0) got += r;
        } else {
            sched_yield();
        }
    }
    return NULL;
}

/* Flips flags on the write end and asks terminal questions of it. */
static void *flipper(void *arg) {
    long me = (long)arg;
    for (int i = 0; i < ROUNDS; i++) {
        int on = (i + me) & 1;
        if (ioctl(pfd[1], on ? FIOCLEX : FIONCLEX) != 0) __atomic_add_fetch(&seen_bad, 1, __ATOMIC_RELAXED);
        struct termios t;
        struct winsize w;
        if (ioctl(pfd[1], TCGETS, &t) == -1 && errno == ENOTTY) __atomic_add_fetch(&notty, 1, __ATOMIC_RELAXED);
        if (ioctl(pfd[1], TIOCGWINSZ, &w) == -1 && errno == ENOTTY) __atomic_add_fetch(&notty, 1, __ATOMIC_RELAXED);
        __atomic_add_fetch(&flips, 1, __ATOMIC_RELAXED);
    }
    return NULL;
}

int main(void) {
    if (pipe(pfd) != 0) return 1;
    pthread_t w, r, f[3];
    pthread_create(&w, NULL, writer, NULL);
    pthread_create(&r, NULL, reader, NULL);
    for (long i = 0; i < 3; i++) pthread_create(&f[i], NULL, flipper, (void *)i);
    pthread_join(w, NULL);
    pthread_join(r, NULL);
    for (int i = 0; i < 3; i++) pthread_join(f[i], NULL);

    /* Settled from one thread: each request's effect is the descriptor's. */
    ioctl(pfd[1], FIOCLEX);
    int cloexec = fcntl(pfd[1], F_GETFD) & FD_CLOEXEC;
    ioctl(pfd[1], FIONCLEX);
    int cleared = fcntl(pfd[1], F_GETFD) & FD_CLOEXEC;
    int one = 1, zero = 0;
    ioctl(pfd[0], FIONBIO, &one);
    int nb = (fcntl(pfd[0], F_GETFL) & O_NONBLOCK) != 0;
    char c;
    errno = 0;
    int empty = read(pfd[0], &c, 1) == -1 && errno == EAGAIN;
    ioctl(pfd[0], FIONBIO, &zero);
    int blocking = (fcntl(pfd[0], F_GETFL) & O_NONBLOCK) == 0;
    int n = -1;
    write(pfd[1], "xyz", 3);
    ioctl(pfd[0], FIONREAD, &n);

    printf("bad %ld\nnotty %ld\nflips %ld\n", seen_bad, notty, flips);
    printf("cloexec %d cleared %d\nnonblock %d empty %d blocking %d\nfionread %d\n", cloexec != 0, cleared != 0, nb,
           empty, blocking, n);
    return 0;
}
