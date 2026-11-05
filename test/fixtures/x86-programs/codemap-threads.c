/* Code mapped from a file by several threads at once, while others map and
 * unmap anonymous memory: what happens after a syscall that mapped code --
 * export providers bound, replaced libraries looked up, units wanted,
 * objects queued for recompiling -- and a dispatch miss in code mapped
 * from a file keep the lock order.
 * Each thread maps a copy of this program, named like a shared object, and
 * calls a function in the copy: it answers as the original does, wherever
 * the copy lands and however often the addresses are reused. Run under the
 * page with GEX_ELF_UNITS=all or GEX_RECOMPILE_OBJECTS=1 the same way. */
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#define ROUNDS 60
#define THREADS 3
static long bad;
static char dir[64], lib[96];
static long size, leaf_off;

#if UINTPTR_MAX > 0xffffffff
typedef Elf64_Phdr Phdr;
#else
typedef Elf32_Phdr Phdr;
#endif

static void fail(const char *what) {
    if (__atomic_add_fetch(&bad, 1, __ATOMIC_RELAXED) < 5) fprintf(stderr, "bad: %s\n", what);
}

/* No absolute references: it runs the same from any copy. */
__attribute__((noinline)) int leaf(int x) {
    int r = 1;
    for (int i = 0; i < (x & 7) + 1; i++) r = r * 3 + x;
    return r;
}

/* Map the copy, call its `leaf`, give it back. */
static void *caller(void *arg) {
    long id = (long)arg;
    for (int i = 0; i < ROUNDS; i++) {
        int fd = open(lib, O_RDONLY);
        if (fd < 0) { fail("open"); continue; }
        unsigned char *p = mmap(NULL, size, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0);
        close(fd);
        if (p == MAP_FAILED) { fail("mmap"); continue; }
        int (*f)(int) = (int (*)(int))(p + leaf_off);
        for (int k = 0; k < 8; k++) {
            int x = (int)(id * 1000 + i * 8 + k);
            if (f(x) != leaf(x)) fail("the copy answered otherwise");
        }
        if (munmap(p, size)) fail("munmap");
    }
    return NULL;
}

/* Anonymous churn beside it, into the addresses the copies leave. */
static void *churn(void *arg) {
    (void)arg;
    for (int i = 0; i < ROUNDS * 8; i++) {
        size_t len = 0x3000 + 0x1000 * (i % 5);
        unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { fail("anon mmap"); continue; }
        p[len - 1] = 1;
        if (munmap(p, len)) fail("anon munmap");
    }
    return NULL;
}

int main(void) {
    /* Where `leaf` is in the file: its executable segment's offset. */
    const Phdr *ph = (const Phdr *)getauxval(AT_PHDR);
    long n = (long)getauxval(AT_PHNUM);
    uintptr_t at = (uintptr_t)&leaf, bias = 0;
    leaf_off = -1;
    for (long i = 0; i < n; i++)
        if (ph[i].p_type == PT_PHDR) bias = (uintptr_t)ph - ph[i].p_vaddr;
    for (long i = 0; i < n; i++)
        if (ph[i].p_type == PT_LOAD && (ph[i].p_flags & PF_X) && at >= bias + ph[i].p_vaddr && at < bias + ph[i].p_vaddr + ph[i].p_filesz)
            leaf_off = (long)(at - bias - ph[i].p_vaddr + ph[i].p_offset);
    if (leaf_off < 0) { fprintf(stderr, "leaf not found\n"); return 1; }
    /* The copy, named as a shared object is: this program's file rebuilt
     * from its loaded segments (the page has no /proc/self/exe). */
    /* Its own directory: concurrent runs under gexc share /tmp, and the
     * guest's pid is the same in each. */
    snprintf(dir, sizeof dir, "/tmp/codemap-XXXXXX");
    if (!mkdtemp(dir)) return 1;
    snprintf(lib, sizeof lib, "%s/libcodemap.so", dir);
    int out = open(lib, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (out < 0) { fprintf(stderr, "create: errno %d\n", errno); return 1; }
    for (long i = 0; i < n; i++) {
        if (ph[i].p_type != PT_LOAD) continue;
        const char *from = (const char *)(bias + ph[i].p_vaddr);
        if (pwrite(out, from, ph[i].p_filesz, ph[i].p_offset) != (ssize_t)ph[i].p_filesz) { fprintf(stderr, "write\n"); return 1; }
        if ((long)(ph[i].p_offset + ph[i].p_filesz) > size) size = (long)(ph[i].p_offset + ph[i].p_filesz);
    }
    close(out);
    pthread_t c[THREADS], a[2];
    for (long i = 0; i < THREADS; i++) pthread_create(&c[i], NULL, caller, (void *)i);
    for (long i = 0; i < 2; i++) pthread_create(&a[i], NULL, churn, NULL);
    for (int i = 0; i < THREADS; i++) pthread_join(c[i], NULL);
    for (int i = 0; i < 2; i++) pthread_join(a[i], NULL);
    unlink(lib);
    rmdir(dir);
    printf("rounds %d bad %ld\n", ROUNDS, bad);
    return bad != 0;
}
