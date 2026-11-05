/* setjmp/longjmp in a wasm32 program: clang lowers them onto wasm
 * exception handling (needs an engine with it: wasmtime 49 natively). */
#include <setjmp.h>
#include <stdio.h>

static jmp_buf jb;
static int depth;

__attribute__((noinline)) static void dive(int n)
{
    depth = n;
    if (n == 5) longjmp(jb, 42);
    dive(n + 1);
}

int main(void)
{
    volatile int count = 0;
    int r = setjmp(jb);
    count++;
    if (!r) dive(0);
    printf("longjmp returned %d after depth %d, setjmp returned %d times\n", r, depth, count);
    return 0;
}
