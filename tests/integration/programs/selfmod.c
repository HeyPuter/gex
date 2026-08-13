/* A program that writes its own machine code three ways, so a translation
 * cached against a guest *address* must be invalidated when the bytes there
 * change:
 *
 *   1. mmap a page, write, execute.
 *   2. unmap it and map a fresh one at the same address.
 *   3. keep the page and rewrite it in place, flipping W^X and announcing it
 *      with `ic ivau`.
 *
 * Each round returns a different constant, so a stale translation shows up as
 * the previous round's answer rather than as a crash.
 */
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

/* `mov w0, #n ; ret` */
static void emit_return(unsigned int *code, int n) {
    code[0] = 0x52800000u | ((unsigned int)n << 5); /* mov w0, #n */
    code[1] = 0xd65f03c0u;                          /* ret        */
}

/* Tell the processor the instructions changed, exactly as a code generator
 * must: clean the data cache to the point of unification, invalidate the
 * instruction cache, then a barrier before the first call. */
static void publish(void *from, unsigned long bytes) {
    char *p = (char *)((unsigned long)from & ~63UL);
    char *end = (char *)from + bytes;
    for (; p < end; p += 64) {
        __asm__ volatile("dc cvau, %0" ::"r"(p) : "memory");
    }
    __asm__ volatile("dsb ish" ::: "memory");
    p = (char *)((unsigned long)from & ~63UL);
    for (; p < end; p += 64) {
        __asm__ volatile("ic ivau, %0" ::"r"(p) : "memory");
    }
    __asm__ volatile("dsb ish; isb" ::: "memory");
}

typedef int (*fn)(void);

int main(void) {
    long page = sysconf(_SC_PAGESIZE);

    /* Round one: a fresh page. */
    void *slot = mmap(NULL, page, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (slot == MAP_FAILED) {
        printf("mmap failed\n");
        return 1;
    }
    emit_return(slot, 11);
    publish(slot, 8);
    if (mprotect(slot, page, PROT_READ | PROT_EXEC) != 0) {
        printf("mprotect failed\n");
        return 1;
    }
    printf("fresh page returns %d\n", ((fn)slot)());

    /* Round two: the same address, a different mapping. */
    if (munmap(slot, page) != 0) {
        printf("munmap failed\n");
        return 1;
    }
    void *again = mmap(slot, page, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (again != slot) {
        printf("could not remap the same address\n");
        return 1;
    }
    emit_return(again, 22);
    publish(again, 8);
    if (mprotect(again, page, PROT_READ | PROT_EXEC) != 0) {
        printf("mprotect failed\n");
        return 1;
    }
    printf("remapped page returns %d\n", ((fn)again)());

    /* Round three: the same mapping, rewritten in place. */
    if (mprotect(again, page, PROT_READ | PROT_WRITE) != 0) {
        printf("mprotect back failed\n");
        return 1;
    }
    emit_return(again, 33);
    publish(again, 8);
    if (mprotect(again, page, PROT_READ | PROT_EXEC) != 0) {
        printf("mprotect failed\n");
        return 1;
    }
    printf("rewritten page returns %d\n", ((fn)again)());

    return 0;
}
