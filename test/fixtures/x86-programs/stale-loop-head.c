/* A loop whose head is on one page and whose body is on the next, the head's
 * page rewritten (its translations retired) while the body's page is left
 * alone, then the loop run again many times.
 *
 * FL Studio's page startup translated one such head about 100 times a second
 * (34k translations in 290 s) until the renderer ran out of memory. The body,
 * still translated in the module that held the old head, jumps straight to
 * its own copy of the head, which fails its page check (PAGE_STALE). The
 * check took "the same stale block twice in a row" for a block dispatch
 * keeps returning to, and translated the head again on every iteration,
 * although dispatch already reached the new head; the body jumped to the old
 * one again regardless. Here that is 200,000 translations, far past the
 * test's time limit; resumed through dispatch it is quick.
 *
 * The code is position-independent and the same bytes in 32- and 64-bit
 * mode: push edi; xor eax, eax; mov edi, N; jmp head ... head: add eax, K;
 * jmp body ... body: dec edi; jnz head; pop edi; ret. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#define PAGE 4096
#define ENTRY 0x40
#define N 200000

static unsigned char *code;
typedef int (*fn_t)(void);

static void put32(unsigned char *at, int32_t v) {
    memcpy(at, &v, 4);
}

static void head(int32_t k) {
    unsigned char *h = code;
    h[0] = 0x05; /* add eax, imm32 */
    put32(h + 1, k);
    h[5] = 0xe9; /* jmp body */
    put32(h + 6, PAGE - 10);
}

static int run(void) {
    fn_t f = (fn_t)(uintptr_t)(code + ENTRY);
    return f();
}

int main(void) {
#ifdef MAP_32BIT
    int low = MAP_32BIT;
#else
    int low = 0;
#endif
    code = mmap(0, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | low, -1, 0);
    if (code == MAP_FAILED) return 2;
    memset(code, 0x90, 2 * PAGE);
    head(3);
    unsigned char *e = code + ENTRY;
    e[0] = 0x57;                   /* push edi */
    e[1] = 0x31; e[2] = 0xc0;      /* xor eax, eax */
    e[3] = 0xbf; put32(e + 4, N);  /* mov edi, N */
    e[8] = 0xe9; put32(e + 9, -(ENTRY + 13)); /* jmp head */
    unsigned char *b = code + PAGE;
    b[0] = 0xff; b[1] = 0xcf;      /* dec edi */
    b[2] = 0x0f; b[3] = 0x85; put32(b + 4, -(PAGE + 8)); /* jnz head */
    b[8] = 0x5f;                   /* pop edi */
    b[9] = 0xc3;                   /* ret */
    if (mprotect(code, 2 * PAGE, PROT_READ | PROT_EXEC)) return 3;
    printf("first: %d\n", run());
    printf("again: %d\n", run());
    /* The head's page only: writable (its translations retired), a new
     * constant, executable again. The body's page keeps its translation. */
    for (int k = 5; k <= 7; k += 2) {
        if (mprotect(code, PAGE, PROT_READ | PROT_WRITE)) return 4;
        head(k);
        if (mprotect(code, PAGE, PROT_READ | PROT_EXEC)) return 5;
        printf("head %d: %d\n", k, run());
        printf("head %d again: %d\n", k, run());
    }
    return 0;
}
