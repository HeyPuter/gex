/* I/O into buffers above 4 GiB: a file, a pipe and
 * a socketpair read into and written from anonymous memory in the high arena.
 * In the browser these bytes cross between the process's shim and the global
 * kernel module by address, which was 32-bit before the wasm64 shim. Under
 * `gexc run --mem64` (test/x86-programs passes it for mem64-*); with a wasm32 memory, or
 * natively, the buffers are low and the output is the same. x86-64 only. */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

#define LEN 100000

static unsigned sum(const unsigned char *p, size_t n) {
    unsigned h = 2166136261u;
    for (size_t i = 0; i < n; i++) h = (h ^ p[i]) * 16777619u;
    return h;
}

static char *high(size_t n) {
    char *p = mmap(NULL, n, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p == MAP_FAILED ? NULL : p;
}

int main(void) {
    char *a = high(1 << 20), *b = high(1 << 20);
    if (!a || !b) { puts("mmap failed"); return 1; }
    for (int i = 0; i < LEN; i++) a[i] = (char)(i * 7 + i / 251);
    unsigned want = sum((unsigned char *)a, LEN);

    /* A file: write from a, read back into b. */
    /* A name of its own: concurrent runs under gexc share /tmp. */
    char path[] = "/tmp/mem64-io-XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) { puts("open failed"); return 1; }
    if (write(fd, a, LEN) != LEN) { puts("file write short"); return 1; }
    memset(b, 0, LEN);
    if (pread(fd, b, LEN, 0) != LEN) { puts("file read short"); return 1; }
    printf("file %s\n", sum((unsigned char *)b, LEN) == want ? "ok" : "MISMATCH");
    close(fd);
    unlink(path);

    /* A pipe, in chunks the pipe buffer takes. */
    int p[2];
    if (pipe(p) != 0) { puts("pipe failed"); return 1; }
    memset(b, 0, LEN);
    size_t done = 0;
    while (done < LEN) {
        size_t n = LEN - done < 4096 ? LEN - done : 4096;
        if (write(p[1], a + done, n) != (ssize_t)n) { puts("pipe write short"); return 1; }
        size_t got = 0;
        while (got < n) {
            ssize_t r = read(p[0], b + done + got, n - got);
            if (r <= 0) { puts("pipe read failed"); return 1; }
            got += r;
        }
        done += n;
    }
    printf("pipe %s\n", sum((unsigned char *)b, LEN) == want ? "ok" : "MISMATCH");

    /* A socketpair. */
    int s[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, s) != 0) { puts("socketpair failed"); return 1; }
    memset(b, 0, 4096);
    if (write(s[0], a, 4096) != 4096) { puts("socket write short"); return 1; }
    size_t got = 0;
    while (got < 4096) {
        ssize_t r = read(s[1], b + got, 4096 - got);
        if (r <= 0) { puts("socket read failed"); return 1; }
        got += r;
    }
    printf("socket %s\n", sum((unsigned char *)b, 4096) == sum((unsigned char *)a, 4096) ? "ok" : "MISMATCH");
    return 0;
}
