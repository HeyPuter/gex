/* A load through a pointer far outside the address space ends the process
 * with SIGSEGV, as natively. Under gex the address is steered past the end
 * of guest memory and the engine traps; with more than one module
 * instantiated in the thread's store (every runtime translation is one),
 * wasmtime 27 asserted instead of reporting the trap, and the host panicked.
 * The loop makes sure several translations exist before the fault.
 *
 * x86-64 only: an i386 pointer cannot leave the address space, and an
 * unmapped page inside it reads as memory under gex (no page protection),
 * so the i386 build just exits. */
#include <stdio.h>
#include <stdint.h>

#if UINTPTR_MAX > 0xffffffffu
static volatile uintptr_t where = (uintptr_t)0x123456789000ull;
#endif

__attribute__((noinline)) static int step(int i) { return i * 3 + 1; }

int main(void) {
    int acc = 0;
    for (int i = 0; i < 100; i++) acc += step(i);
    printf("before %d\n", acc);
    fflush(stdout);
#if UINTPTR_MAX > 0xffffffffu
    return *(volatile int *)where;
#else
    return 0;
#endif
}
