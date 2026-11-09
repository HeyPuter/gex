/* Where a 64-bit program is placed and what it is told about memory
 * (test/wow64/exeplace, test/wine/mem64spawn). Prints its base,
 * GlobalMemoryStatusEx's ullAvailVirtual and ullTotalVirtual, the free
 * address space and largest free block of the user range as VirtualQuery
 * shows them, the state of the page at 0x90000000 (between 2 and 4 GiB,
 * gex's own space in a 64-bit page memory), then "exeplace done".
 * gex-nt-arch: x64
 * gex-nt-variant: exeplace -Wl,--dynamicbase,--high-entropy-va
 * gex-nt-variant: exeplace-fixed -Wl,--disable-dynamicbase,--disable-reloc-section
 */
#include <windows.h>
#include <stdio.h>

extern IMAGE_DOS_HEADER __ImageBase;

int main(void)
{
    MEMORYSTATUSEX ms = { sizeof(ms) };
    SYSTEM_INFO si;
    MEMORY_BASIC_INFORMATION mbi;
    ULONGLONG free = 0, largest = 0;
    char *p, *end;
    DWORD mid;

    GetSystemInfo( &si );
    if (!GlobalMemoryStatusEx( &ms )) { printf( "exeplace GlobalMemoryStatusEx failed %lu\n", GetLastError() ); return 1; }
    p = si.lpMinimumApplicationAddress;
    end = si.lpMaximumApplicationAddress;
    while (p < end && VirtualQuery( p, &mbi, sizeof(mbi) ))
    {
        char *next = (char *)mbi.BaseAddress + mbi.RegionSize;
        if (next <= p) break;
        if (next > end) next = end + 1;
        if (mbi.State == MEM_FREE)
        {
            ULONGLONG n = next - (char *)mbi.BaseAddress;
            free += n;
            if (n > largest) largest = n;
        }
        p = next;
    }
    mid = VirtualQuery( (void *)0x90000000, &mbi, sizeof(mbi) ) ? mbi.State : 0;
    printf( "exeplace base %p avail %I64u total %I64u free %I64u largest %I64u mid %lx\n",
            (void *)&__ImageBase, ms.ullAvailVirtual, ms.ullTotalVirtual, free, largest, mid );
    printf( "exeplace done\n" );
    return 0;
}
