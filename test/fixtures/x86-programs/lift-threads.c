/* Translations from several threads at once, beside threads in the kernel.
 * Four threads each build a fresh chain of small blocks every round (`add
 * eax, K; jmp next`, the last block of each page running onto the next one,
 * K their own), run it, check it adds up and unmap it, which retires its
 * translations while the others are lifting theirs. Two more threads stay in
 * the kernel meanwhile (getpid, a pipe written and read, sched_yield) and
 * must keep getting answers.
 *
 * In the page a translation's lift and install take no kernel lock: the JIT state goes out with one lifter at a
 * time, the others wait for it with the JIT lock given back, and a retire
 * that lands during a lift is retired again after the install. Every chain
 * must add up and every thread end; how many kernel calls were answered
 * during a lift goes to stderr (natively there is hardly a lift to be in). */
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define LIFTERS 4
#define BLOCKS 3000
#define STRIDE 16
#define ROUNDS 6
/* 13 bytes into every 16: the last block of each page has its `add` in the
   page's last three bytes and its `jmp` on the next page. */
#define FIRST 13

static atomic_int lifting;
static atomic_int done;
static atomic_long answered_while_lifting;
static int pipefd[2];

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

static void *lifter(void *arg) {
    int k = (int)(intptr_t)arg;
    size_t size = (FIRST + BLOCKS * STRIDE + 1 + 4095) & ~(size_t)4095;
    for (int r = 0; r < ROUNDS; r++) {
        unsigned char *code = map_code(size);
        if (!code) return (void *)1;
        memset(code, 0xcc, size);
        code[0] = 0x31; code[1] = 0xc0;      /* xor eax, eax */
        code[2] = 0xeb; code[3] = FIRST - 4; /* jmp to the first block */
        for (int i = 0; i < BLOCKS; i++) {
            unsigned char *b = code + FIRST + i * STRIDE;
            b[0] = 0x83; b[1] = 0xc0; b[2] = (unsigned char)(k + r); /* add eax, k+r */
            int32_t rel = STRIDE - 8;                                 /* jmp to the next block */
            b[3] = 0xe9;
            memcpy(b + 4, &rel, 4);
        }
        code[FIRST + BLOCKS * STRIDE] = 0xc3; /* ret */
        if (mprotect(code, size, PROT_READ | PROT_EXEC) != 0) return (void *)1;
        int (*chain)(void) = (int (*)(void))(void *)code;
        atomic_fetch_add(&lifting, 1);
        int got = chain();
        atomic_fetch_sub(&lifting, 1);
        if (got != BLOCKS * (k + r)) {
            printf("lifter %d round %d: the chain added up to %d, not %d\n", k, r, got, BLOCKS * (k + r));
            return (void *)2;
        }
        /* And again, translated now: the same answer. */
        if (chain() != BLOCKS * (k + r)) return (void *)3;
        if (munmap(code, size) != 0) return (void *)1;
    }
    return 0;
}

static void *kernel_user(void *arg) {
    int which = (int)(intptr_t)arg;
    char c = 'x';
    while (!atomic_load(&done)) {
        int in_lift = atomic_load(&lifting) > 0;
        if (which == 0) {
            if (getpid() <= 0) return (void *)1;
            sched_yield();
        } else {
            if (write(pipefd[1], &c, 1) != 1) return (void *)1;
            if (read(pipefd[0], &c, 1) != 1 || c != 'x') return (void *)2;
        }
        if (in_lift && atomic_load(&lifting) > 0) atomic_fetch_add(&answered_while_lifting, 1);
    }
    return 0;
}

int main(void) {
    if (pipe(pipefd) != 0) return 2;
    pthread_t ks[2], ls[LIFTERS];
    for (int i = 0; i < 2; i++) pthread_create(&ks[i], 0, kernel_user, (void *)(intptr_t)i);
    for (int i = 0; i < LIFTERS; i++) pthread_create(&ls[i], 0, lifter, (void *)(intptr_t)(i + 1));
    int bad = 0;
    for (int i = 0; i < LIFTERS; i++) {
        void *ret;
        pthread_join(ls[i], &ret);
        if (ret) {
            printf("lifter %d failed (%ld)\n", i + 1, (long)ret);
            bad = 1;
        }
    }
    atomic_store(&done, 1);
    for (int i = 0; i < 2; i++) {
        void *ret;
        pthread_join(ks[i], &ret);
        if (ret) {
            printf("kernel thread %d failed (%ld)\n", i, (long)ret);
            bad = 1;
        }
    }
    /* How many were answered mid-lift is not printed: natively a chain runs
       in microseconds and the count is often zero. */
    fprintf(stderr, "%ld kernel calls answered while a chain was lifting\n", atomic_load(&answered_while_lifting));
    if (!bad) printf("%d lifters, %d rounds each, every chain added up\n", LIFTERS, ROUNDS);
    return bad;
}
