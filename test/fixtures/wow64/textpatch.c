/* textpatch: the guest of test/wow64/textpatch. Patches its own code the
 * way an old MinGW runtime applies pseudo-relocations: for each patch, the
 * region holding the target (VirtualQuery's: all of .text) made writable
 * and executable, four bytes written, the region given its old protection
 * back, in a function of its own (the relocator's __write_memory). The
 * code doing it is in that region, and runs on after each VirtualProtect
 * returns and after each write returns to its caller's loop. Each patch
 * changes what `getter` returns.
 *
 *   textpatch N   N patches; says "textpatch before 1234567 after N-1"
 *                 (the last patch seen), or "textpatch skip" when the
 *                 compiler did not make `getter` `mov eax, imm32; ret` */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

__attribute__((noinline, aligned(4096))) int getter(void) { return 1234567; }

__attribute__((noinline)) static int write_memory(void *addr, const void *src, size_t len) {
    MEMORY_BASIC_INFORMATION b;
    DWORD old;
    if (!VirtualQuery(addr, &b, sizeof b)) return 0;
    if (!VirtualProtect(b.BaseAddress, b.RegionSize, PAGE_EXECUTE_READWRITE, &old)) return 0;
    memcpy(addr, src, len);
    return VirtualProtect(b.BaseAddress, b.RegionSize, old, &old);
}

__attribute__((noinline)) int patch_all(unsigned char *f, int n) {
    for (int i = 0; i < n; i++)
        if (!write_memory(f + 1, &i, 4)) return 0;
    return 1;
}

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 300;
    unsigned char *f = (unsigned char *)getter;
    MEMORY_BASIC_INFORMATION b;
    if (f[0] != 0xb8 || f[5] != 0xc3 || !VirtualQuery(f, &b, sizeof b) ||
        (unsigned char *)patch_all < (unsigned char *)b.BaseAddress ||
        (unsigned char *)patch_all >= (unsigned char *)b.BaseAddress + b.RegionSize) {
        printf("textpatch skip\n");
        return 0;
    }
    int (*volatile call)(void) = getter;
    int before = call();
    if (!patch_all(f, n)) {
        printf("textpatch VirtualProtect failed %lu\n", GetLastError());
        return 1;
    }
    printf("textpatch before %d after %d\n", before, call());
    fflush(stdout);
    return 0;
}
