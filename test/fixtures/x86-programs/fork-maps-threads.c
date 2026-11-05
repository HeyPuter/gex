/* Forks while other threads change the memory map (the map is read for
 * a fork's image, for naming code and for the translator's file reads
 * without the kernel lock, through the process's published map).
 *
 * - Two threads map, fill, check, protect and unmap anonymous regions over
 *   and over, and one maps a file's pages and reads them back.
 * - The main thread forks repeatedly. Each child checks a region the parent
 *   mapped before the threads started (its pattern, and that a page made
 *   read-only stays readable), runs code from a function the parent has run,
 *   and exits with how many checks failed.
 * - The parent waits for each child and counts the failures. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define FORKS 40
#define REGION (64 * 4096)

static atomic_int stop, bad;
static unsigned char *fixed;
static char dir[64] = "/tmp/fork-maps-threads-XXXXXX", file_path[96];

static void fail(const char *what, long n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%ld)\n", what, n);
}

/* Code the parent runs before forking and each child runs after. */
static __attribute__((noinline)) unsigned mix(unsigned x) {
    for (int i = 0; i < 16; i++) x = x * 2654435761u + (unsigned)i;
    return x;
}

static void *churn(void *arg) {
    int id = (int)(long)arg;
    for (unsigned round = 0; !atomic_load(&stop); round++) {
        size_t len = (size_t)(1 + (round + id) % 16) * 4096;
        unsigned char *p = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { fail("mmap", (long)round); continue; }
        memset(p, (int)(round + id), len);
        if (p[len - 1] != (unsigned char)(round + id)) fail("anon fill", (long)round);
        if (mprotect(p, 4096, PROT_READ)) fail("mprotect", (long)round);
        if (p[0] != (unsigned char)(round + id)) fail("read after mprotect", (long)round);
        if (munmap(p, len)) fail("munmap", (long)round);
    }
    return 0;
}

static void *files(void *arg) {
    (void)arg;
    int fd = open(file_path, O_RDONLY);
    if (fd < 0) { fail("open file", 0); return 0; }
    for (unsigned round = 0; !atomic_load(&stop); round++) {
        unsigned char *p = mmap(0, 2 * 4096, PROT_READ, MAP_PRIVATE, fd, 0);
        if (p == MAP_FAILED) { fail("file mmap", (long)round); continue; }
        if (p[0] != 'F' || p[4096 + 7] != (unsigned char)(7 * 3)) fail("file bytes", (long)round);
        munmap(p, 2 * 4096);
    }
    close(fd);
    return 0;
}

static int child(unsigned want) {
    int n = 0;
    for (size_t i = 0; i < REGION; i += 997)
        if (fixed[i] != (unsigned char)(i * 7)) { n++; break; }
    /* The last page was made read-only before the threads started. */
    if (fixed[REGION - 1] != (unsigned char)((REGION - 1) * 7)) n++;
    if (mix(7) != want) n++;
    return n;
}

int main(void) {
    /* Its own directory: concurrent runs under gexc share /tmp, and the
     * guest's pid is the same in each. */
    if (!mkdtemp(dir)) return 2;
    snprintf(file_path, sizeof file_path, "%s/f", dir);
    int fd = open(file_path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) return 2;
    unsigned char page[2 * 4096];
    for (int i = 0; i < (int)sizeof page; i++) page[i] = (unsigned char)((i % 4096) * 3);
    page[0] = 'F';
    if (write(fd, page, sizeof page) != (ssize_t)sizeof page) return 2;
    close(fd);

    fixed = mmap(0, REGION, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (fixed == MAP_FAILED) return 2;
    for (size_t i = 0; i < REGION; i++) fixed[i] = (unsigned char)(i * 7);
    if (mprotect(fixed + REGION - 4096, 4096, PROT_READ)) return 2;
    unsigned want = mix(7);

    pthread_t t[3];
    pthread_create(&t[0], 0, churn, (void *)0L);
    pthread_create(&t[1], 0, churn, (void *)1L);
    pthread_create(&t[2], 0, files, 0);
    for (int i = 0; i < FORKS; i++) {
        pid_t c = fork();
        if (c < 0) { fail("fork", i); continue; }
        if (c == 0) _exit(child(want));
        int st = 0;
        if (waitpid(c, &st, 0) != c || !WIFEXITED(st)) fail("child ended", st);
        else if (WEXITSTATUS(st) != 0) fail("child's checks", WEXITSTATUS(st));
    }
    atomic_store(&stop, 1);
    for (int i = 0; i < 3; i++) pthread_join(t[i], 0);
    unlink(file_path);
    rmdir(dir);
    printf("forks %d bad %d\n", FORKS, atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
