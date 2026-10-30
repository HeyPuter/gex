/* oom: the guest of test/page/oom.test.js. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/wait.h>
/* Fill a file in the kernel's filesystem until a write fails, then check the
 * kernel still serves a new process: the errno, or -1 when it never failed. */
static int tmpfs(long cap_mib) {
    static char blk[1 << 20];
    memset(blk, 0x5a, sizeof blk);
    int fd = open("/tmp/oom-fill", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (fd < 0) { printf("FAIL tmpfs: open: %s\n", strerror(errno)); return -2; }
    long n = 0; int err = 0;
    for (;;) {
        if (write(fd, blk, sizeof blk) != (ssize_t)sizeof blk) { err = errno; break; }
        if (++n > cap_mib * 2) return -1;
    }
    close(fd);
    unlink("/tmp/oom-fill");
    pid_t c = fork();
    if (!c) { int f = open("/tmp/oom-after", O_CREAT | O_WRONLY, 0600); _exit(f < 0 || write(f, "x", 1) != 1); }
    int st;
    if (c < 0 || waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st)) { printf("FAIL tmpfs: a process after the failed write: %#x\n", c < 0 ? -1 : st); return -2; }
    printf("ok tmpfs %ld (%s)\n", n, strerror(err));
    return 0;
}
/* Allocate blocks of `size` and touch them until malloc fails: the MiB held. */
/* Kept and summed, so the compiler cannot drop the malloc and the memset. */
static char *volatile last;
static volatile long sum;
static long fill(size_t size, long cap_mib) {
    long n = 0;
    for (;;) {
        char *p = malloc(size);
        if (!p) break;
        memset(p, 0x5a, size);
        *(char **)p = last;
        last = p;
        sum += p[size - 1];
        n++;
        if ((long)((n * size) >> 20) > cap_mib * 2) return -1; /* never refused */
    }
    return (long)((n * size) >> 20);
}
static long cap;
static void *thread(void *arg) {
    return (void *)fill((size_t)arg, cap);
}
int main(int argc, char **argv) {
    cap = atol(argv[2]);
    long got = 0;
    if (!strcmp(argv[1], "tmpfs")) { int r = tmpfs(cap); if (r == -1) printf("FAIL tmpfs: write never failed\n"); return r != 0; }
    if (!strcmp(argv[1], "big")) got = fill(1 << 20, cap);
    else if (!strcmp(argv[1], "small")) got = fill(4000, cap);
    else if (!strcmp(argv[1], "threads")) {
        /* A thread out of memory may not be made at all (EAGAIN, as Linux
         * refuses one it has no stack for): only those made are joined. */
        pthread_t t[4];
        int made[4];
        for (int i = 0; i < 4; i++) made[i] = !pthread_create(&t[i], 0, thread, (void *)(size_t)(i & 1 ? 4000 : 1 << 20));
        for (int i = 0; i < 4; i++) { void *r; if (!made[i]) continue; pthread_join(t[i], &r); if ((long)r < 0) got = -1; else if (got >= 0) got += (long)r; }
    }
    if (got < 0) { printf("FAIL %s: malloc never failed\n", argv[1]); return 1; }
    printf("ok %s %ld\n", argv[1], got);
    return 0;
}
