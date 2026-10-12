/* Code retired while other code is being translated. One thread runs a long
 * chain of small blocks (8,000 of them, each `add eax, 1; jmp next`, the last
 * of each page running onto the next one), which
 * takes many dispatch misses and long lifts to translate; another keeps
 * flipping a separate code page (a function it also calls) to
 * read-write-execute and back, rewriting a byte with the value it already
 * holds. Every flip retires that page's translations (`retire_code`), and in
 * the page the flips land while the chain is being lifted outside the kernel
 * lock (docs/kernel-locking.md, step 5). The chain must add up every round
 * and the run must end. A unit dropped for a retire once left the pages it
 * had numbered with no directory entry, and a block running onto one of them
 * then failed its page check for ever: Undertale froze at its first frame, and
 * this froze the same way (2 runs of 2, a build dropping on every retire).
 *
 * Not named *smc*: run.sh would run it under --fjit as well, which livelocks
 * natively once a chain page is retired (a separate bug).
 *
 * Mostly the flipped page is not the chain's: a page retired faster than
 * its own code can be lifted never runs, here or natively ("its page kept
 * being retired"). Every tenth flip is one chain page, slowly enough. */
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>

#define BLOCKS 8000
#define STRIDE 16
#define ROUNDS 20
/* Where the first block starts: 13 bytes into every 16, so the last block of
   each page has its `add` in the page's last three bytes and its `jmp` on the
   next page. */
#define FIRST 13
#ifndef FLIP_US
#define FLIP_US 200
#endif

static unsigned char *flip_page;
static unsigned char *chain_code;
static size_t chain_size;
static atomic_int done;
static atomic_long flips;

static void spin(long us) {
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    do clock_gettime(CLOCK_MONOTONIC, &b);
    while ((b.tv_sec - a.tv_sec) * 1000000L + (b.tv_nsec - a.tv_nsec) / 1000 < us);
}

/* MAP_32BIT: a 64-bit guest memory keeps executable code below 4 GiB. */
static unsigned char *map_code(size_t size) {
#ifdef MAP_32BIT
    int low = MAP_32BIT;
#else
    int low = 0;
#endif
    void *p = mmap(0, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | low, -1, 0);
    return p == MAP_FAILED ? 0 : p;
}

static void *flipper(void *arg) {
    (void)arg;
    int (*seven)(void) = (int (*)(void))(void *)flip_page;
    while (!atomic_load(&done)) {
        if (mprotect(flip_page, 4096, PROT_READ | PROT_WRITE | PROT_EXEC) != 0) return (void *)1;
        /* The same byte back: the code does not change, its translations go. */
        volatile unsigned char *b = flip_page + 1;
        *b = *b;
        if (mprotect(flip_page, 4096, PROT_READ | PROT_EXEC) != 0) return (void *)1;
        if (seven() != 7) return (void *)2;
        long n = atomic_fetch_add(&flips, 1);
        /* Now and then a page of the chain itself, rewritten the same way,
           slowly enough that the chain's lifts still finish between. */
        if (n % 10 == 5) {
            unsigned char *page = chain_code + ((size_t)(n / 10) * 4096) % chain_size;
            if (mprotect(page, 4096, PROT_READ | PROT_WRITE | PROT_EXEC) != 0) return (void *)1;
            volatile unsigned char *c = page + 7;
            *c = *c;
            if (mprotect(page, 4096, PROT_READ | PROT_EXEC) != 0) return (void *)1;
        }
        spin(FLIP_US);
    }
    return 0;
}

int main(void) {
    size_t size = (FIRST + BLOCKS * STRIDE + 1 + 4095) & ~(size_t)4095;
    unsigned char *code = map_code(size);
    flip_page = map_code(4096);
    if (!code || !flip_page) return 2;
    memset(code, 0xcc, size);
    code[0] = 0x31; code[1] = 0xc0;      /* xor eax, eax */
    code[2] = 0xeb; code[3] = FIRST - 4; /* jmp to the first block */
    for (int i = 0; i < BLOCKS; i++) {
        unsigned char *b = code + FIRST + i * STRIDE;
        b[0] = 0x83; b[1] = 0xc0; b[2] = 0x01; /* add eax, 1 */
        int32_t rel = STRIDE - 8;               /* jmp to the next block */
        b[3] = 0xe9;
        memcpy(b + 4, &rel, 4);
    }
    code[FIRST + BLOCKS * STRIDE] = 0xc3; /* ret */
    memset(flip_page, 0xcc, 4096);
    memcpy(flip_page, "\xb8\x07\x00\x00\x00\xc3", 6); /* mov eax, 7; ret */
    if (mprotect(code, size, PROT_READ | PROT_EXEC) != 0) return 2;
    if (mprotect(flip_page, 4096, PROT_READ | PROT_EXEC) != 0) return 2;
    int (*chain)(void) = (int (*)(void))(void *)code;
    chain_code = code;
    chain_size = size;
    pthread_t t;
    pthread_create(&t, 0, flipper, 0);
    /* Flipping before the first lift starts. */
    while (atomic_load(&flips) < 2) spin(10);
    int bad = 0;
    for (int r = 0; r < ROUNDS; r++) {
        int got = chain();
        if (got != BLOCKS) {
            printf("round %d: the chain added up to %d, not %d\n", r, got, BLOCKS);
            bad = 1;
            break;
        }
        long f0 = atomic_load(&flips);
        while (atomic_load(&flips) < f0 + 2) spin(10);
    }
    atomic_store(&done, 1);
    void *ret;
    pthread_join(t, &ret);
    if (ret) {
        printf("the flipper failed (%ld)\n", (long)ret);
        return 1;
    }
    if (!bad) printf("%d rounds, every chain added up to %d\n", ROUNDS, BLOCKS);
    return bad;
}
