/* Code one thread translated, run by another thread that reaches it later:
 * each Worker instantiates a module of the process only when its thread
 * first wants a block of it. Thread A makes NFUNC functions on pages of
 * their own and calls each; thread B, started after, calls two of them,
 * one of which A has rewritten in between (so the module B would have
 * found is dead), and one A rewrites after B first ran it. B must always
 * get what the bytes say now. Prints "ok" and exits 0, or what was wrong. */
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#define NFUNC 64

static unsigned char *code;
static volatile int step;

static void put(int i, int n) {
    unsigned char c[6] = {0xb8, 0, 0, 0, 0, 0xc3}; /* mov eax, n; ret */
    memcpy(c + 1, &n, 4);
    memcpy(code + 0x1000 * i, c, sizeof c);
    __asm__ volatile("" ::: "memory");
}

static int call(int i) { return ((int (*)(void))(code + 0x1000 * i))(); }

static void wait_for(int s) {
    while (__atomic_load_n(&step, __ATOMIC_ACQUIRE) < s) sched_yield();
}

static void *b_thread(void *arg) {
    (void)arg;
    long bad = 0;
    wait_for(1);
    /* 1: rewritten by A before B ever ran it */
    if (call(1) != 1001) { printf("B: function 1 gave %d, not 1001\n", call(1)); bad++; }
    /* 2: run, rewritten by A, run again */
    if (call(2) != 2) { printf("B: function 2 gave %d, not 2\n", call(2)); bad++; }
    __atomic_store_n(&step, 2, __ATOMIC_RELEASE);
    wait_for(3);
    if (call(2) != 2002) { printf("B: function 2 gave %d after its rewrite, not 2002\n", call(2)); bad++; }
    return (void *)bad;
}

int main(void) {
    code = mmap(0, 0x1000 * NFUNC, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (code == MAP_FAILED) return 2;
    int bad = 0;
    for (int i = 0; i < NFUNC; i++) {
        put(i, i);
        if (call(i) != i) bad++;
    }
    pthread_t b;
    if (pthread_create(&b, 0, b_thread, 0)) return 3;
    put(1, 1001);
    if (call(1) != 1001) bad++;
    __atomic_store_n(&step, 1, __ATOMIC_RELEASE);
    wait_for(2);
    put(2, 2002);
    if (call(2) != 2002) bad++;
    __atomic_store_n(&step, 3, __ATOMIC_RELEASE);
    void *r;
    pthread_join(b, &r);
    bad += (int)(long)r;
    if (bad) { printf("%d wrong\n", bad); return 1; }
    printf("ok\n");
    return 0;
}
