/* A W64 program asks for more address space than its wasm32 memory has: the
 * mapping fails, and gexc does not record the program as one that needs a
 * 64-bit memory (a lowered program cannot use one; tests/wasm64/run.sh). */
#include <stdio.h>
#include <sys/mman.h>

int main(void)
{
    void *p = mmap(NULL, 3UL << 30, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    void *q = p == MAP_FAILED ? MAP_FAILED : mmap(NULL, 3UL << 30, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    printf("6 GiB of mappings refused: %d\n", p == MAP_FAILED || q == MAP_FAILED);
    return 0;
}
