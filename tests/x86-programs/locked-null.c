/* A locked access through a null pointer's field faults (SIGSEGV), as on
 * hardware. gex has no page protection below the guest's floor -- its own
 * thread contexts and dispatch directory live there -- so such a write once
 * landed silently in the runtime's memory; the translators now check the
 * address of every locked access (and cmpxchg16b) against
 * gex_abi::GUEST_FLOOR. Misaligned (0x39) first, then aligned; the first
 * one ends the program. */
#include <stdio.h>
#include <stdint.h>

static volatile uintptr_t where = 0x39;

int main(void) {
    printf("before\n");
    fflush(stdout);
    __atomic_fetch_add((int *)where, 1, __ATOMIC_SEQ_CST);
    printf("after a misaligned locked add at %#lx\n", (unsigned long)where);
    where = 0x40;
    __atomic_fetch_add((int *)where, 1, __ATOMIC_SEQ_CST);
    printf("after an aligned locked add at %#lx\n", (unsigned long)where);
    return 0;
}
