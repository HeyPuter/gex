/* A block that runs across a page boundary, the second page rewritten
 * (its translations retired) more times than the stale page check's bound
 * (STALE_AGAIN_MAX, 64), the first page left alone, the block called after
 * each rewrite.
 *
 * Dispatch at the block's start reads the first page's directory entry,
 * which still names the block: the block fails its check on the second
 * page and asks to be translated again (PAGE_STALE). Each rewrite is new
 * code, so each asks once. The bound counted those asks for the whole run:
 * past 64 the block resumed from JavaScript, dispatch came straight back to
 * the same stale block, and the thread never moved again. FL Studio's page
 * start: Wine's ACM codecs loaded one after another at 0x63b00000, block
 * 0x63b01ff0, the loader lock held by the thread that stopped.
 *
 * The code: nops from 0xff0, `mov eax, K` at 0xfff with its whole
 * immediate on the second page, `add eax, 1`, `ret`. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#define PAGE 4096
#define AT (PAGE - 0x10)
#define ROUNDS 150
#define CALLS 1000

typedef int (*fn_t)(void);

int main(void) {
#ifdef MAP_32BIT
    int low = MAP_32BIT;
#else
    int low = 0;
#endif
    unsigned char *code = mmap(0, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | low, -1, 0);
    if (code == MAP_FAILED) return 2;
    memset(code, 0xcc, 2 * PAGE);
    memset(code + AT, 0x90, 15);
    code[PAGE - 1] = 0xb8; /* mov eax, imm32: the immediate at PAGE */
    code[PAGE + 4] = 0x83; code[PAGE + 5] = 0xc0; code[PAGE + 6] = 0x01; /* add eax, 1 */
    code[PAGE + 7] = 0xc3; /* ret */
    fn_t f = (fn_t)(uintptr_t)(code + AT);
    long wrong = 0;
    for (int round = 0; round < ROUNDS; round++) {
        int32_t k = 1000 * (round + 1);
        if (round && mprotect(code + PAGE, PAGE, PROT_READ | PROT_WRITE)) return 3;
        memcpy(code + PAGE, &k, 4);
        if (mprotect(round ? code + PAGE : code, round ? PAGE : 2 * PAGE, PROT_READ | PROT_EXEC)) return 4;
        for (int i = 0; i < CALLS; i++)
            if (f() != k + 1) wrong++;
        if (round % 50 == 0 || round == ROUNDS - 1) printf("round %d: %d, %ld wrong so far\n", round, f(), wrong);
    }
    return 0;
}
