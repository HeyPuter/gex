/* A W^X JIT in a 64-bit guest memory (docs/mem64.md §3): code written into
 * a read-write mapping, which gex places above 4 GiB, then made executable
 * with mprotect and called; rewritten and flipped again (the old
 * translation must go); and an executable MAP_FIXED mapping at 11 GiB.
 * Before the M64 dense directory, both were refused with EACCES. */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

typedef int (*fn)(void);

/* mov eax, imm32; ret */
static void emit(unsigned char *p, int v) {
    p[0] = 0xb8;
    memcpy(p + 1, &v, 4);
    p[5] = 0xc3;
}

int main(void) {
    unsigned char *p = mmap(0, 0x2000, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 2; }
    printf("high %d\n", (uintptr_t)p >= (1ull << 32));
    emit(p, 42);
    emit(p + 0x1000, 7);
    if (mprotect(p, 0x2000, PROT_READ | PROT_EXEC)) { perror("mprotect rx"); return 3; }
    volatile fn f = (fn)p, g = (fn)(p + 0x1000);
    printf("call %d %d\n", f(), g());
    if (mprotect(p, 0x1000, PROT_READ | PROT_WRITE)) { perror("mprotect rw"); return 4; }
    emit(p, 43);
    if (mprotect(p, 0x1000, PROT_READ | PROT_EXEC)) { perror("mprotect rx again"); return 5; }
    printf("again %d %d\n", f(), g());
    void *want = (void *)(11ull << 30);
    unsigned char *q = mmap(want, 0x1000, PROT_READ | PROT_WRITE | PROT_EXEC,
                            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (q == MAP_FAILED) { perror("mmap fixed rwx"); return 6; }
    emit(q, 99);
    printf("fixed %d %d\n", q == want, ((fn)q)());
    return 0;
}
