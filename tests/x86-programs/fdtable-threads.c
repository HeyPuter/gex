/* The descriptor table's own calls from several threads at once, answered
 * without the kernel lock (docs/kernel-locking.md, L10): dup, dup2, dup3,
 * fcntl (F_DUPFD, F_SETFD/F_GETFD, F_SETFL/F_GETFL), flock and close
 * racing on one table, while a writer feeds a pipe through descriptors
 * the others keep duplicating and closing. Every byte arrives, the write
 * end's last close is the reader's EOF, a flock is released by its
 * description's last close and not before, and the counts come out the
 * same as on Linux. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/file.h>
#include <unistd.h>

#define ROUNDS 3000
#define CHURNERS 3
static int pfd[2];
static long bad;

static void fail(const char *what) {
    __atomic_add_fetch(&bad, 1, __ATOMIC_RELAXED);
    if (__atomic_load_n(&bad, __ATOMIC_RELAXED) < 5) fprintf(stderr, "bad: %s (errno %d)\n", what, errno);
}

/* Each byte goes through a fresh copy of the write end, closed at once. */
static void *writer(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS; i++) {
        int w = dup(pfd[1]);
        if (w < 0) { fail("dup write end"); continue; }
        char c = (char)i;
        if (write(w, &c, 1) != 1) fail("write");
        if (close(w) != 0) fail("close copy");
    }
    return NULL;
}

/* Copies of the read end made every way there is, their flags flipped and
 * read back, then closed. Each thread's copies are its own. */
static void *churn(void *arg) {
    int id = (int)(long)arg;
    for (int i = 0; i < ROUNDS; i++) {
        int a = dup(pfd[0]);
        int b = fcntl(pfd[0], F_DUPFD_CLOEXEC, 100 + id * 50);
        int c = dup3(pfd[0], 300 + id, O_CLOEXEC);
        int d = dup2(pfd[0], 400 + id);
        if (a < 0 || b < 100 || c != 300 + id || d != 400 + id) fail("dup family");
        if (fcntl(b, F_GETFD) != FD_CLOEXEC || fcntl(c, F_GETFD) != FD_CLOEXEC) fail("cloexec set");
        if (fcntl(a, F_GETFD) != 0 || fcntl(d, F_GETFD) != 0) fail("cloexec clear");
        if (fcntl(a, F_SETFD, i & 1) != 0 || fcntl(a, F_GETFD) != (i & 1)) fail("F_SETFD");
        if (fcntl(d, F_GETFL) < 0) fail("F_GETFL");
        if (close(a) || close(b) || close(c) || close(d)) fail("close");
        if (close(c) != -1 || errno != EBADF) fail("double close");
    }
    return NULL;
}

/* Two descriptions of one file: the second may lock only once every
 * descriptor of the first is closed. */
static void *locker(void *arg) {
    const char *path = arg;
    long held = 0;
    for (int i = 0; i < ROUNDS / 10; i++) {
        int x = open(path, O_RDWR);
        int y = open(path, O_RDWR);
        if (x < 0 || y < 0) { fail("open"); continue; }
        if (flock(x, LOCK_EX | LOCK_NB) != 0) fail("lock x");
        int x2 = dup(x);
        if (flock(y, LOCK_EX | LOCK_NB) == 0) held++;
        close(x);
        if (flock(y, LOCK_EX | LOCK_NB) == 0) held++;
        close(x2);
        if (flock(y, LOCK_EX | LOCK_NB) != 0) fail("lock after last close");
        close(y);
    }
    if (held) fail("locked while held");
    return NULL;
}

int main(void) {
    char path[] = "/tmp/fdtable-XXXXXX";
    int t = mkstemp(path);
    if (t < 0 || pipe(pfd) != 0) return 1;
    close(t);
    pthread_t w, l, c[CHURNERS];
    pthread_create(&w, NULL, writer, NULL);
    pthread_create(&l, NULL, locker, path);
    for (long i = 0; i < CHURNERS; i++) pthread_create(&c[i], NULL, churn, (void *)i);
    long got = 0;
    char buf[512];
    int closed = 0;
    for (;;) {
        ssize_t r = read(pfd[0], buf, sizeof buf);
        if (r < 0) { fail("read"); break; }
        if (r == 0) break;
        for (ssize_t i = 0; i < r; i++)
            if (buf[i] != (char)(got + i)) { fail("order"); break; }
        got += r;
        /* Everything written: drop our write end, the writer's copies are
         * all closed, so the next read is EOF. */
        if (got == ROUNDS && !closed) {
            pthread_join(w, NULL);
            close(pfd[1]);
            closed = 1;
        }
    }
    for (int i = 0; i < CHURNERS; i++) pthread_join(c[i], NULL);
    pthread_join(l, NULL);
    unlink(path);
    if (fcntl(pfd[1], F_GETFD) != -1 || errno != EBADF) fail("write end still open");
    printf("bytes %ld eof %d bad %ld\n", got, closed, bad);
    return bad != 0;
}
