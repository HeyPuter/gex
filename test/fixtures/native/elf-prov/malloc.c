/* ELF providers' conformance: glibc's heap (malloc, free, calloc, realloc,
 * memalign, aligned_alloc, posix_memalign, valloc, pvalloc,
 * malloc_usable_size). Random allocation traces with checksummed contents,
 * alignments, zeroing, errors and errno, the library's own allocations
 * (strdup, fopen, getline), threads, and a child that frees its parent's
 * blocks. Prints only what does not depend on where blocks are. */
#define _GNU_SOURCE
#include <errno.h>
#include <malloc.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static uint64_t rng = 88172645463325252ull;
static uint64_t next(void) { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; return rng; }

static void fill(unsigned char *p, size_t n, unsigned seed) { for (size_t i = 0; i < n; ++i) p[i] = (unsigned char)(seed + i * 31); }
static int check(const unsigned char *p, size_t n, unsigned seed) { for (size_t i = 0; i < n; ++i) if (p[i] != (unsigned char)(seed + i * 31)) return 0; return 1; }

#define N 2000
static void *blk[N];
static size_t len[N];
static unsigned seed[N];

static int trace(unsigned rounds) {
    int bad = 0;
    for (unsigned r = 0; r < rounds; ++r) {
        unsigned i = next() % N;
        unsigned op = next() % 6;
        size_t n = (next() % 4 == 0) ? next() % 70000 : next() % 300;
        if (!blk[i]) {
            blk[i] = op == 5 ? calloc(1, n) : malloc(n);
            if (!blk[i]) return -1;
            if (op == 5) for (size_t k = 0; k < n; ++k) bad += ((unsigned char *)blk[i])[k] != 0;
            bad += ((uintptr_t)blk[i] & 15) != 0;
            bad += malloc_usable_size(blk[i]) < n;
            len[i] = n, seed[i] = (unsigned)next();
            fill(blk[i], n, seed[i]);
        } else if (op < 3) {
            bad += !check(blk[i], len[i], seed[i]);
            free(blk[i]);
            blk[i] = 0;
        } else {
            bad += !check(blk[i], len[i], seed[i]);
            void *q = realloc(blk[i], n);
            if (!q && n) return -1;
            size_t keep = len[i] < n ? len[i] : n;
            bad += !check(q, keep, seed[i]);
            blk[i] = q, len[i] = n;
            fill(q, n, seed[i]);
            if (!n) { free(q); blk[i] = 0; }
        }
    }
    return bad;
}

static void *thr(void *arg) {
    uintptr_t t = (uintptr_t)arg;
    int bad = 0;
    void *mine[256] = {0};
    for (unsigned r = 0; r < 20000; ++r) {
        unsigned i = (r * 7 + t) % 256;
        if (mine[i]) {
            bad += ((unsigned char *)mine[i])[0] != (unsigned char)(i + t);
            free(mine[i]);
            mine[i] = 0;
        } else {
            mine[i] = malloc(1 + (r * 13 + t) % 500);
            ((unsigned char *)mine[i])[0] = (unsigned char)(i + t);
        }
    }
    for (int i = 0; i < 256; ++i) free(mine[i]);
    return (void *)(uintptr_t)bad;
}

int main(void) {
    printf("trace: bad %d\n", trace(200000));
    /* Alignment. */
    int bad = 0;
    for (size_t al = 1; al <= 65536; al *= 2) {
        void *p = memalign(al, 100), *q = aligned_alloc(al, 200), *r = 0;
        int e = posix_memalign(&r, al, 300);
        bad += ((uintptr_t)p % al) != 0 || ((uintptr_t)q % al) != 0;
        if (al >= sizeof(void *)) bad += e != 0 || ((uintptr_t)r % al) != 0;
        else bad += e != EINVAL;
        free(p), free(q), free(e ? 0 : r);
    }
    void *r = (void *)1;
    printf("align: bad %d; posix_memalign(24) %d, ptr kept %d\n", bad, posix_memalign(&r, 24, 10), r == (void *)1);
    void *v = valloc(100), *pv = pvalloc(100);
    printf("valloc page-aligned %d, pvalloc %d usable>=4096 %d\n", ((uintptr_t)v & 4095) == 0, ((uintptr_t)pv & 4095) == 0, malloc_usable_size(pv) >= 4096);
    free(v), free(pv);
    /* Errors. */
    errno = 0;
    /* Past PTRDIFF_MAX: glibc fails it without asking the kernel. */
    void *huge = malloc(((size_t)-1 >> 1) + 1);
    printf("huge malloc %d errno %s\n", huge == 0, errno == ENOMEM ? "ENOMEM" : "other");
    errno = 0;
    size_t big = (size_t)1 << 40;
    huge = calloc(big, big + (size_t)(errno != 0));
    printf("overflowing calloc %d errno %s\n", huge == 0, errno == ENOMEM ? "ENOMEM" : "other");
    printf("realloc(p, 0) %d; free(NULL) ok; usable(NULL) %zu\n", realloc(malloc(10), 0) == 0, malloc_usable_size(0));
    free(0);
    void *z = malloc(0);
    printf("malloc(0) non-null %d\n", z != 0);
    free(z);
    /* The library's own allocations. */
    char *d = strdup("a string the library copies");
    FILE *f = fopen("/proc/self/stat", "r");
    char *line = 0;
    size_t cap = 0;
    ssize_t got = f ? getline(&line, &cap, f) : -1;
    printf("strdup %s; getline %d\n", d, got > 0);
    free(line), free(d);
    if (f) fclose(f);
    char *as = 0;
    int an = asprintf(&as, "%d-%s", 42, "x");
    printf("asprintf %d [%s]\n", an, as);
    free(as);
    /* Threads. */
    pthread_t th[8];
    for (uintptr_t t = 0; t < 8; ++t) pthread_create(&th[t], 0, thr, (void *)t);
    int tbad = 0;
    for (int t = 0; t < 8; ++t) { void *x; pthread_join(th[t], &x); tbad += (int)(uintptr_t)x; }
    printf("threads: bad %d\n", tbad);
    /* A child frees and reallocates its parent's blocks. */
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        int cb = trace(20000);
        for (int i = 0; i < N; ++i) free(blk[i]);
        printf("child: bad %d\n", cb);
        fflush(stdout);
        _exit(0);
    }
    int st;
    waitpid(c, &st, 0);
    printf("parent after child: bad %d\n", trace(20000));
    return 0;
}
