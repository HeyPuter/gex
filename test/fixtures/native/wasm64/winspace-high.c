/* A program marked as running x86-64 Windows code (the 64-bit Wine loader) maps past
 * 2 GiB: 40 mappings of 64 MiB, each written at both ends. 32-bit Wine's compact
 * address space ends at 2 GiB; a 64-bit Wine process takes the NT layer's split
 * instead, as it did before 32-bit Wine had the compact one (Hades x64's page ran out
 * of memory at 2 GiB). test/page/wasm64-page expects this output, not a
 * native run's. */
#include <stdio.h>
#include <sys/mman.h>

int main(void)
{
    unsigned long total = 0, high = 0;
    for (int i = 0; i < 40; i++) {
        char *p = mmap(0, 64 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED)
            break;
        p[0] = 1;
        p[(64 << 20) - 1] = 1;
        total += 64;
        if ((unsigned long)p + (64 << 20) > 0x80000000UL)
            high = 1;
    }
    printf("mapped %lu MiB, above 2 GiB %lu\n", total, high);
    return 0;
}
