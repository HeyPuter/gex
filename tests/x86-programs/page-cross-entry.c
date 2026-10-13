/* A function whose entry is a page's last byte, so its first instruction runs
 * onto the next page, called before and after that next page changes.
 *
 * Paint's startup stalled for ever (1 run in about 85, scratch/P/stall85) on
 * uiribbon's function at 0x604eefff (`mov edi, edi` across the boundary):
 * tier-1 translated its block again and again, each translation failing its
 * page check against the next page's directory entry. A block that runs onto a
 * page checks that page's entry as well as its own, so whatever leaves that
 * entry unlike the page table's base turns every entry into a translation.
 *
 * Here the next page is made writable and rewritten (its translations
 * retired, its entry cleared), unmapped and mapped afresh, and left alone
 * while the first page changes; after each the function must return the new
 * value, and the run must end. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#define PAGE 4096

static unsigned char *code;
typedef int (*fn_t)(void);

/* At the first page's last byte: `mov edi, edi` (8b ff), `mov eax, imm32`
 * (b8 ...), `ret`. All but the first byte are on the second page, which is
 * all this writes. */
static void put(int v) {
    code[PAGE + 0] = 0xff;
    code[PAGE + 1] = 0xb8;
    memcpy(code + PAGE + 2, &v, 4);
    code[PAGE + 6] = 0xc3;
}

static int calls(int n) {
    fn_t f = (fn_t)(uintptr_t)(code + PAGE - 1);
    int last = 0;
    for (int i = 0; i < n; i++) last = f();
    return last;
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
    code[PAGE - 1] = 0x8b;
    put(41);
    mprotect(code, 2 * PAGE, PROT_READ | PROT_EXEC);
    printf("first: %d\n", calls(1000));

    /* The second page rewritten: its entry cleared, the block stale. */
    mprotect(code + PAGE, PAGE, PROT_READ | PROT_WRITE);
    put(42);
    mprotect(code + PAGE, PAGE, PROT_READ | PROT_EXEC);
    printf("second page rewritten: %d\n", calls(1000));

    /* The second page unmapped and mapped again with new code. */
    munmap(code + PAGE, PAGE);
    void *q = mmap(code + PAGE, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (q != code + PAGE) return 3;
    memset(q, 0x90, PAGE);
    put(43);
    mprotect(code + PAGE, PAGE, PROT_READ | PROT_EXEC);
    printf("second page mapped again: %d\n", calls(1000));

    /* The first page rewritten (the same byte), the second left alone. */
    mprotect(code, PAGE, PROT_READ | PROT_WRITE);
    code[PAGE - 1] = 0x8b;
    mprotect(code, PAGE, PROT_READ | PROT_EXEC);
    printf("first page rewritten: %d\n", calls(1000));

    /* Both, alternately, many times. */
    int ok = 1;
    for (int r = 0; r < 50; r++) {
        mprotect(code + PAGE, PAGE, PROT_READ | PROT_WRITE);
        put(100 + r);
        mprotect(code + PAGE, PAGE, PROT_READ | PROT_EXEC);
        if (calls(20) != 100 + r) ok = 0;
    }
    printf("rounds: %s\n", ok ? "ok" : "wrong");
    return 0;
}
