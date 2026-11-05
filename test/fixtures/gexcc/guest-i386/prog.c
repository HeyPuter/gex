/* test/gexcc/guest-i386's program: i386 code of a wasm32 program's own
 * (-fgex-guest-i386), both ways and rewritten. Prints one line per check. */
#include <gex/i386.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

/* int f(int a, int b): a + b, or a - b (one byte apart, at [4]) */
static const unsigned char ADD[] = {0x8b, 0x44, 0x24, 0x04, 0x03, 0x44, 0x24, 0x08, 0xc3};
static const unsigned char SUB[] = {0x8b, 0x44, 0x24, 0x04, 0x2b, 0x44, 0x24, 0x08, 0xc3};

static uint32_t add_at;

static int call2(uint32_t f, uint32_t a, uint32_t b)
{
    uint32_t w[2] = {a, b};
    return (int)(uint32_t)gex_i386_call(f, w, 2);
}

/* the program's side of a call from i386 code: a * b + (a + b), the sum
 * from i386 code again (nested) */
GEX_I386_ENTRY(mul_entry)
{
    uint32_t a = gex_i386_arg(ctx, 0), b = gex_i386_arg(ctx, 1);
    return gex_i386_return(ctx, ret, 0, a * b + (uint32_t)call2(add_at, a, b));
}

static unsigned char *page(int prot)
{
    void *p = mmap(0, 4096, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p == MAP_FAILED ? 0 : p;
}

/* `code` at `p`, W^X: writable, then executable */
static void wx(unsigned char *p, const unsigned char *code, size_t n)
{
    mprotect(p, 4096, PROT_READ | PROT_WRITE);
    memcpy(p, code, n);
    mprotect(p, 4096, PROT_READ | PROT_EXEC);
}

/* ---- another thread runs the code while this one rewrites it ---- */
#define ROUNDS 200
static unsigned char *code;
/* gen: the round whose code is there; ack: the last round the runner
 * checked; out: the runner is outside the code (W^X: the writer waits for
 * it, as the page is not executable while written) */
static atomic_int gen, ack, stop, in_code, hold;
static int wrong, rwx;

static void *runner(void *arg)
{
    (void)arg;
    int seen = 0;
    while (!atomic_load(&stop)) {
        int g = atomic_load_explicit(&gen, memory_order_acquire);
        if (!rwx) {
            atomic_store(&in_code, 1);
            if (atomic_load(&hold)) {
                atomic_store(&in_code, 0);
                continue;
            }
        }
        if (g != seen) {
            /* round g's code: SUB for odd, ADD for even */
            if (call2((uint32_t)(uintptr_t)code, 7, 2) != (g % 2 ? 5 : 9))
                wrong++;
            seen = g;
            atomic_store_explicit(&ack, g, memory_order_release);
        } else {
            /* between rounds: its translation kept hot */
            (void)call2((uint32_t)(uintptr_t)code, 1, 1);
        }
        if (!rwx)
            atomic_store(&in_code, 0);
    }
    return 0;
}

static int rounds(int mode)
{
    pthread_t t;
    rwx = mode;
    wrong = 0;
    atomic_store(&gen, 0);
    atomic_store(&ack, 0);
    atomic_store(&stop, 0);
    atomic_store(&hold, 0);
    code = page(rwx ? PROT_READ | PROT_WRITE | PROT_EXEC : PROT_READ | PROT_WRITE);
    memcpy(code, ADD, sizeof ADD);
    if (!rwx)
        mprotect(code, 4096, PROT_READ | PROT_EXEC);
    pthread_create(&t, 0, runner, 0);
    for (int g = 1; g <= ROUNDS; g++) {
        const unsigned char *c = g % 2 ? SUB : ADD;
        if (rwx) {
            /* stored while the other thread runs the code */
            atomic_store_explicit((atomic_uchar *)&code[4], c[4], memory_order_relaxed);
        } else {
            atomic_store(&hold, 1);
            while (atomic_load(&in_code))
                ;
            wx(code, c, sizeof ADD);
            atomic_store(&hold, 0);
        }
        atomic_store_explicit(&gen, g, memory_order_release);
        while (atomic_load_explicit(&ack, memory_order_acquire) != g)
            ;
    }
    atomic_store(&stop, 1);
    pthread_join(t, 0);
    munmap(code, 4096);
    return wrong;
}

int main(void)
{
    unsigned char *a = page(PROT_READ | PROT_WRITE), *c = page(PROT_READ | PROT_WRITE);
    if (!a || !c) {
        puts("mmap failed");
        return 1;
    }
    /* the program calls i386 code */
    wx(a, ADD, sizeof ADD);
    add_at = (uint32_t)(uintptr_t)a;
    printf("call %d\n", call2(add_at, 2, 3));

    /* i386 code calls the program, which calls i386 code again:
     * push [esp+8]; push [esp+8]; mov eax, mul; call eax; add esp, 8; ret */
    uint32_t mul = (uint32_t)(uintptr_t)mul_entry;
    if (gex_i386_bind(mul, mul_entry)) {
        puts("bind refused");
        return 1;
    }
    unsigned char cb[] = {0xff, 0x74, 0x24, 0x08, 0xff, 0x74, 0x24, 0x08, 0xb8, 0, 0, 0, 0,
                          0xff, 0xd0, 0x83, 0xc4, 0x08, 0xc3};
    memcpy(cb + 9, &mul, 4);
    wx(c, cb, sizeof cb);
    printf("callback %d\n", call2((uint32_t)(uintptr_t)c, 6, 7));

    /* rewritten W^X */
    wx(a, SUB, sizeof SUB);
    printf("rewritten %d\n", call2(add_at, 9, 4));
    wx(a, ADD, sizeof ADD);

    /* rewritten in place, in a page both writable and executable */
    unsigned char *r = page(PROT_READ | PROT_WRITE | PROT_EXEC);
    memcpy(r, ADD, sizeof ADD);
    int before = call2((uint32_t)(uintptr_t)r, 9, 4);
    r[4] = SUB[4];
    printf("rwx %d %d\n", before, call2((uint32_t)(uintptr_t)r, 9, 4));

    /* another thread's translation of code this thread rewrites */
    printf("thread-wx %d wrong of %d\n", rounds(0), ROUNDS);
    printf("thread-rwx %d wrong of %d\n", rounds(1), ROUNDS);
    return 0;
}
