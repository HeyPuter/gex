/* Code written through one view of a section and run through another (a
 * JIT that keeps its code pages read+execute and writes them through a
 * read+write view of the same section: .NET's W^X). Each round writes a
 * function returning the round's number through the writable view, flushes
 * the instruction cache and calls it through the executable one, and the
 * call has to return the new number. Prints "dualmap ok N" or the first
 * round that ran old code. argv[1] "rwfirst" maps the writable view first. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#define ROUNDS 300

/* mov eax, imm32; ret */
static void put(unsigned char *p, DWORD v)
{
    p[0] = 0xb8;
    memcpy(p + 1, &v, 4);
    p[5] = 0xc3;
}

int main(int argc, char **argv)
{
    BOOL rwfirst = argc > 1 && !strcmp(argv[1], "rwfirst");
    HANDLE s = CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_EXECUTE_READWRITE, 0, 0x10000, NULL);
    unsigned char *rw = NULL, *rx = NULL;
    int i, bad = -1;
    DWORD got = 0;

    if (!s)
    {
        printf("dualmap CreateFileMapping %lu\n", GetLastError());
        return 1;
    }
    if (rwfirst) rw = MapViewOfFile(s, FILE_MAP_WRITE, 0, 0, 0x10000);
    rx = MapViewOfFile(s, FILE_MAP_READ | FILE_MAP_EXECUTE, 0, 0, 0x10000);
    if (!rwfirst) rw = MapViewOfFile(s, FILE_MAP_WRITE, 0, 0, 0x10000);
    if (!rw || !rx)
    {
        printf("dualmap MapViewOfFile %p %p %lu\n", rw, rx, GetLastError());
        return 1;
    }
    for (i = 0; i < ROUNDS; i++)
    {
        /* the same function each time, and every tenth round one at a new
         * place in the same page */
        unsigned off = (i / 10) * 16 % 0x1000;
        DWORD (*fn)(void) = (DWORD (*)(void))(rx + off);
        put(rw + off, 1000 + i);
        FlushInstructionCache(GetCurrentProcess(), rx + off, 6);
        if ((got = fn()) != 1000 + (DWORD)i)
        {
            bad = i;
            break;
        }
    }
    if (bad >= 0) printf("dualmap stale at round %d: %lu, not %d\n", bad, got, 1000 + bad);
    else printf("dualmap ok %d\n", ROUNDS);
    return bad >= 0;
}
