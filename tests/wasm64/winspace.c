/* A program marked as running x86-64 Windows code (wcc64
 * -Wl,--gex-guest-x86-64: the W64 loader) gets Windows' address space:
 * the user shared data's address (0x7ffe0000) and the page after it free
 * to map, and anonymous mappings below them. tests/wasm64/run.sh expects
 * this output, not a native run's. */
#include <stdio.h>
#include <sys/mman.h>

int main(void)
{
    char *p = mmap(0, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    void *usd = mmap((void *)0x7ffe0000, 0x2000, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    printf("anonymous mapping below the USD: %d\n", p != MAP_FAILED && (unsigned long)(p + (1 << 20)) <= 0x7ffe0000);
    printf("USD and the page after it free: %d\n", usd == (void *)0x7ffe0000);
    return 0;
}
