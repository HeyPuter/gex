/* x86 locked operations as atomics from several threads, where wasm's
 * atomics would not do as they are:
 *   - `cmpxchg8b` that fails stores nothing (a concurrent writer keeps every
 *     increment while another thread spams failing compares at the word);
 *   - `lock xadd` and `lock cmpxchg` on a 4-byte counter misaligned inside
 *     one 8-byte word, and on one straddling two words;
 *   - `lock cmpxchg8b` on a 4-aligned (not 8-aligned) pair.
 * x86 makes all of these atomic; each total must come out exact. The
 * straddling ones are split locks, which a kernel may slow down
 * (split_lock_detect), so they run fewer times. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static unsigned char buf[64] __attribute__((aligned(64)));
#define AT(off) ((volatile uint32_t *)(buf + (off)))

static uint32_t xadd(volatile uint32_t *p, uint32_t v) {
    __asm__ volatile("lock xaddl %0, %1" : "+r"(v), "+m"(*p) : : "memory", "cc");
    return v;
}

static int cas4(volatile uint32_t *p, uint32_t expected, uint32_t want) {
    unsigned char z;
    __asm__ volatile("lock cmpxchgl %3, %1\n sete %0" : "=q"(z), "+m"(*p), "+a"(expected) : "r"(want) : "memory", "cc");
    return z;
}

static int cas8(volatile uint64_t *p, uint64_t expected, uint64_t want, uint64_t *old) {
    uint32_t lo = (uint32_t)expected, hi = (uint32_t)(expected >> 32);
    unsigned char z;
    __asm__ volatile("lock cmpxchg8b %0\n sete %1"
                     : "+m"(*p), "=q"(z), "+a"(lo), "+d"(hi)
                     : "b"((uint32_t)want), "c"((uint32_t)(want >> 32))
                     : "cc", "memory");
    *old = (uint64_t)hi << 32 | lo;
    return z;
}

#define THREADS 4

/* cmpxchg8b that fails must not write: one thread counts, the others fail. */
static volatile uint64_t word __attribute__((aligned(8)));
static volatile int stop;
static void *counter(void *arg) {
    (void)arg;
    for (int i = 0; i < 200000; i++) {
        uint64_t seen = word, old;
        while (!cas8(&word, seen, seen + 1, &old)) seen = old;
    }
    stop = 1;
    return 0;
}
static void *failer(void *arg) {
    (void)arg;
    uint64_t old;
    while (!stop) cas8(&word, 0xdeadbeefcafef00dull, 0, &old);
    return 0;
}

struct job { int off, n, cas; };
static void *bump(void *arg) {
    struct job *j = arg;
    volatile uint32_t *p = AT(j->off);
    for (int i = 0; i < j->n; i++) {
        if (j->cas) {
            uint32_t seen = *p;
            while (!cas4(p, seen, seen + 1)) seen = *p;
        } else {
            xadd(p, 1);
        }
    }
    return 0;
}

static void *bump8(void *arg) {
    int n = *(int *)arg;
    volatile uint64_t *p = (volatile uint64_t *)(buf + 36); /* 4 mod 8 */
    for (int i = 0; i < n; i++) {
        uint64_t seen, old;
        memcpy(&seen, (const void *)p, 8);
        while (!cas8(p, seen, seen + 0x100000001ull, &old)) seen = old;
    }
    return 0;
}

static void run(void *(*f)(void *), void *arg) {
    pthread_t t[THREADS];
    for (int i = 0; i < THREADS; i++) pthread_create(&t[i], 0, f, arg);
    for (int i = 0; i < THREADS; i++) pthread_join(t[i], 0);
}

int main(void) {
    pthread_t c, f[2];
    pthread_create(&c, 0, counter, 0);
    for (int i = 0; i < 2; i++) pthread_create(&f[i], 0, failer, 0);
    pthread_join(c, 0);
    for (int i = 0; i < 2; i++) pthread_join(f[i], 0);
    printf("failing cmpxchg8b: %llu of 200000 increments kept\n", (unsigned long long)word);

    struct job jobs[] = {
        { 1, 50000, 0 },  /* xadd inside one word */
        { 2, 50000, 1 },  /* cmpxchg inside one word */
        { 13, 2000, 0 },  /* xadd across two words */
        { 22, 2000, 1 },  /* cmpxchg across two words */
    };
    for (unsigned k = 0; k < sizeof jobs / sizeof *jobs; k++) {
        memset(buf, 0, sizeof buf);
        run(bump, &jobs[k]);
        printf("%s at +%d: %u of %d\n", jobs[k].cas ? "lock cmpxchg" : "lock xadd", jobs[k].off,
               *AT(jobs[k].off), THREADS * jobs[k].n);
    }
    memset(buf, 0, sizeof buf);
    int n8 = 2000;
    run(bump8, &n8);
    uint64_t v;
    memcpy(&v, buf + 36, 8);
    printf("cmpxchg8b at 4 mod 8: %08x/%08x of %d\n", (uint32_t)(v >> 32), (uint32_t)v, THREADS * n8);
    return 0;
}
