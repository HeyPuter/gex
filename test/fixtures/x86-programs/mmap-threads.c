/* The mapping calls from several threads at once, answered without the
 * kernel lock: mmap, munmap, mprotect,
 * mremap, madvise and msync racing on one memory map, brk asked. Anonymous memory
 * reads as zero however often its addresses are reused; a shared file
 * mapping reaches the file; and code mapped where other code was unmapped
 * a moment before, by another thread, runs as the new code (its
 * translations retired by the unmap), however the addresses are reused. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define ROUNDS 300
#define THREADS 3
static long bad;

static void fail(const char *what) {
    if (__atomic_add_fetch(&bad, 1, __ATOMIC_RELAXED) < 5) fprintf(stderr, "bad: %s (errno %d)\n", what, errno);
}

static int zero(const unsigned char *p, size_t n) {
    for (size_t i = 0; i < n; i++)
        if (p[i]) return 0;
    return 1;
}

/* Map, fill, check, grow, give back. */
static void *anon(void *arg) {
    long id = (long)arg;
    for (int i = 0; i < ROUNDS; i++) {
        size_t len = 0x4000 + 0x1000 * (i % 4);
        unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { fail("mmap"); continue; }
        if (!zero(p, len)) fail("fresh mapping not zero");
        memset(p, (int)(id + 1), len);
        if (madvise(p, 0x1000, MADV_DONTNEED) || !zero(p, 0x1000)) fail("madvise");
        if (mprotect(p + 0x1000, 0x1000, PROT_READ)) fail("mprotect");
        if (p[0x1000] != (unsigned char)(id + 1)) fail("bytes kept");
        if (mprotect(p + 0x1000, 0x1000, PROT_READ | PROT_WRITE)) fail("mprotect back");
        /* One mapping again (Linux merges the pieces), so it moves whole. */
        unsigned char *g = mremap(p, len, len + 0x1000, MREMAP_MAYMOVE);
        if (g == MAP_FAILED) { fail("mremap after mprotect"); munmap(p, len); continue; }
        if (munmap(g, len + 0x1000)) fail("munmap");
        /* Moved while the others map into what it leaves. */
        p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { fail("mmap"); continue; }
        memset(p, (int)(id + 1), len);
        unsigned char *q = mremap(p, len, len * 2, MREMAP_MAYMOVE);
        if (q == MAP_FAILED) { fail("mremap"); munmap(p, len); continue; }
        if (q[0] != (unsigned char)(id + 1) || q[len - 1] != (unsigned char)(id + 1) || !zero(q + len, len)) fail("mremap contents");
        if (munmap(q, len * 2)) fail("munmap moved");
    }
    return NULL;
}

#if defined(__x86_64__) || defined(__i386__)
/* `mov eax, imm32; ret` at a fresh mapping, called: the value it returns is
 * the one just written, whatever code was at that address before. */
static void *jit(void *arg) {
    long id = (long)arg;
    for (int i = 0; i < ROUNDS; i++) {
        /* Executable from the start, as JITs map: in a 64-bit memory only
         * code asked for at mmap is placed below 4 GiB,
         * where mprotect may make it executable again. */
        unsigned char *p = mmap(NULL, 0x1000, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { fail("jit mmap"); continue; }
        int32_t want = (int32_t)(id * 100000 + i);
        p[0] = 0xb8;
        memcpy(p + 1, &want, 4);
        p[5] = 0xc3;
        if (mprotect(p, 0x1000, PROT_READ | PROT_EXEC)) { fail("jit mprotect"); continue; }
        int32_t got = ((int32_t (*)(void))p)();
        if (got != want) fail("stale code ran");
        if (munmap(p, 0x1000)) fail("jit munmap");
    }
    return NULL;
}
#endif

/* A shared file mapping's writes reach the file at msync. */
static void *shared(void *arg) {
    const char *path = arg;
    int fd = open(path, O_RDWR);
    if (fd < 0) { fail("open"); return NULL; }
    for (int i = 0; i < ROUNDS / 3; i++) {
        unsigned char *p = mmap(NULL, 0x2000, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (p == MAP_FAILED) { fail("shared mmap"); continue; }
        p[0x10] = (unsigned char)i;
        if (msync(p, 0x2000, MS_SYNC)) fail("msync");
        unsigned char c = 0xff;
        if (pread(fd, &c, 1, 0x10) != 1 || c != (unsigned char)i) fail("file has the write");
        if (munmap(p, 0x2000)) fail("shared munmap");
    }
    close(fd);
    return NULL;
}

/* The break, asked while the others map: never moves back (malloc may
 * move it up; only it moves it, so this thread only asks). */
static void *heap(void *arg) {
    (void)arg;
    long last = 0;
    for (int i = 0; i < ROUNDS * 4; i++) {
        long b = syscall(SYS_brk, 0);
        if (b <= 0 || b < last) fail("brk");
        last = b;
    }
    return NULL;
}

int main(void) {
    char path[] = "/tmp/mmap-threads-XXXXXX";
    int t = mkstemp(path);
    if (t < 0 || ftruncate(t, 0x2000)) return 1;
    close(t);
    pthread_t a[THREADS], j[THREADS], s, h;
    for (long i = 0; i < THREADS; i++) pthread_create(&a[i], NULL, anon, (void *)i);
#if defined(__x86_64__) || defined(__i386__)
    for (long i = 0; i < THREADS; i++) pthread_create(&j[i], NULL, jit, (void *)i);
#endif
    pthread_create(&s, NULL, shared, path);
    pthread_create(&h, NULL, heap, NULL);
    for (int i = 0; i < THREADS; i++) pthread_join(a[i], NULL);
#if defined(__x86_64__) || defined(__i386__)
    for (int i = 0; i < THREADS; i++) pthread_join(j[i], NULL);
#endif
    pthread_join(s, NULL);
    pthread_join(h, NULL);
    unlink(path);
    printf("rounds %d bad %ld\n", ROUNDS, bad);
    return bad != 0;
}
