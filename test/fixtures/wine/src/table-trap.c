#include <windows.h>
#include <stdio.h>
#include <stdint.h>
/* An indirect call through a dispatch directory entry naming a slot past
 * every table's end: a table trap no catch-up answers.
 * gex-nt-arch: x64
 */
int main(void) {
    unsigned char *p = VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    static const unsigned char code[] = {0xb8, 7, 0, 0, 0, 0xc3};
    memcpy(p, code, sizeof code);
    int (*volatile f)(void) = (int (*)(void))p;
    printf("table-trap first %d\n", f());
    fflush(stdout);
    volatile uint32_t *dir = (volatile uint32_t *)(uintptr_t)0x100000;
    uintptr_t page = (uintptr_t)p / 4096;
    printf("table-trap page %#llx entry %#x\n", (unsigned long long)page, dir[page]);
    fflush(stdout);
    dir[page] = 0x00fff000;
    printf("table-trap second %d\n", f());
    printf("table-trap end\n");
    return 0;
}
