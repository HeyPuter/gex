/* A reservation at a fixed address past 16 GiB, the way MSYS2's and
 * Cygwin's runtime reserves its heap (8 GiB at 32 GiB): no gex memory
 * reaches it, so it fails, and Wine says once why the program cannot run
 * here. Prints the VirtualAlloc result and exits 0. */
#include <windows.h>
#include <stdio.h>

int main(void)
{
    void *want = (void *)((ULONG_PTR)32 << 30), *p;

    p = VirtualAlloc(want, (SIZE_T)8 << 30, MEM_RESERVE, PAGE_READWRITE);
    if (p) printf("reserve32g reserved at %p\n", p);
    else printf("reserve32g refused, error %lu\n", GetLastError());
    fflush(stdout);
    return 0;
}
