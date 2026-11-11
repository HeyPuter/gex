/* highfill: a 64-bit program that reserves every free range of the space
 * between 2 and 4 GiB, as a runtime's heap that takes all it can does,
 * and then loads a DLL of Wine's that the process has not loaded yet.
 *
 * Says "highfill reserved N MiB above 2 GiB, M MiB left" and then
 * "highfill loaded cabinet.dll" or "highfill LoadLibrary failed E". */
#include <windows.h>
#include <stdio.h>

int main(void) {
    const ULONG_PTR lo = 0x80000000ull, hi = 0x100000000ull;
    ULONG_PTR at = lo, got = 0, left = 0;
    while (at < hi) {
        MEMORY_BASIC_INFORMATION mi;
        if (!VirtualQuery((void *)at, &mi, sizeof mi)) break;
        ULONG_PTR end = (ULONG_PTR)mi.BaseAddress + mi.RegionSize;
        if (end > hi) end = hi;
        if (mi.State == MEM_FREE) {
            ULONG_PTR s = (at + 0xffff) & ~(ULONG_PTR)0xffff;
            if (s < end && VirtualAlloc((void *)s, end - s, MEM_RESERVE, PAGE_READWRITE)) got += end - s;
        }
        at = end;
    }
    for (at = lo; at < hi;) {
        MEMORY_BASIC_INFORMATION mi;
        if (!VirtualQuery((void *)at, &mi, sizeof mi)) break;
        ULONG_PTR end = (ULONG_PTR)mi.BaseAddress + mi.RegionSize;
        if (end > hi) end = hi;
        if (mi.State == MEM_FREE) left += end - at;
        at = end;
    }
    printf("highfill reserved %lu MiB above 2 GiB, %lu MiB left\n", (unsigned long)(got >> 20), (unsigned long)(left >> 20));
    fflush(stdout);
    HMODULE m = LoadLibraryA("cabinet.dll");
    if (m) printf("highfill loaded cabinet.dll\n");
    else printf("highfill LoadLibrary failed %lu\n", GetLastError());
    return 0;
}
