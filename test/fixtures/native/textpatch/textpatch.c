/* textpatch: the guest of test/native/textpatch. Patches its own code the
 * way an old MinGW runtime applies pseudo-relocations: for each patch,
 * the section (two pages here) made writable, still executable, four
 * bytes written, the section put back to read+execute. The code doing it
 * is in the section it patches, on the page after the one it patches.
 * Each patch changes what `getter` returns; it is called once before the
 * patches (unless told cold) and once after.
 *
 *   textpatch N [cold]
 *                 N patches; says "textpatch before B after A" (B 1234567,
 *                 A N - 1: the last patch seen), or "textpatch skip" when
 *                 the compiler did not lay the code out so (`getter` as
 *                 `mov eax, imm32; ret`, the loop on the next page) */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

__attribute__((noinline, aligned(4096), section(".text.textpatch"))) int getter(void) { return 1234567; }

__attribute__((noinline, aligned(4096), section(".text.textpatch"))) long patch_loop(int n) {
    unsigned char *f = (unsigned char *)getter;
    void *section = (void *)((uintptr_t)f & ~(uintptr_t)4095);
    for (int i = 0; i < n; i++) {
        if (mprotect(section, 8192, PROT_READ | PROT_WRITE | PROT_EXEC)) return -1;
        memcpy(f + 1, &i, 4);
        if (mprotect(section, 8192, PROT_READ | PROT_EXEC)) return -1;
    }
    return 0;
}

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 300;
    const unsigned char *f = (const unsigned char *)getter;
    if (f[0] != 0xb8 || f[5] != 0xc3 || (uintptr_t)patch_loop != (uintptr_t)f + 4096) {
        printf("textpatch skip\n");
        return 0;
    }
    int (*volatile call)(void) = getter;
    int before = argc > 2 ? 1234567 : call();
    if (patch_loop(n)) {
        printf("textpatch mprotect failed\n");
        return 1;
    }
    printf("textpatch before %d after %d\n", before, call());
    return 0;
}
