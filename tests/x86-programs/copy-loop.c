/* The byte-copy loop `l: movsb; cmp edi, eax; jne l`, which the lifters
 * turn into one string copy (gex_lift_x86::copy_loop): forward, backward
 * (std), overlapping forward (a byte smeared along, as LZ decoders rely
 * on), and the flags the last compare leaves. */
#include <stdio.h>
#include <string.h>

static unsigned long copy(unsigned char *dst, const unsigned char *src, unsigned long n, int back) {
    unsigned long flags;
    unsigned char *end = back ? dst - n : dst + n;
    if (back)
        __asm__ volatile("std");
    __asm__ volatile("1: movsb\n\t"
                     "cmp %%edi, %%eax\n\t"
                     "jne 1b\n\t"
                     "pushf\n\t"
                     "pop %0\n\t"
                     "cld"
                     : "=r"(flags), "+D"(dst), "+S"(src)
                     : "a"(end)
                     : "memory", "cc");
    return flags & 0x8d5;
}

int main(void) {
    unsigned char a[300], b[300];
    for (int i = 0; i < 300; i++)
        a[i] = (unsigned char)(i * 7 + 3);
    memset(b, 0, sizeof b);
    unsigned long f = copy(b + 10, a, 200, 0);
    unsigned sum = 0;
    for (int i = 0; i < 300; i++)
        sum = sum * 31 + b[i];
    printf("forward flags %lx sum %08x\n", f, sum);
    memset(b, 0, sizeof b);
    f = copy(b + 250, a + 280, 100, 1);
    sum = 0;
    for (int i = 0; i < 300; i++)
        sum = sum * 31 + b[i];
    printf("backward flags %lx sum %08x\n", f, sum);
    memcpy(b, a, sizeof b);
    f = copy(b + 3, b, 150, 0);
    sum = 0;
    for (int i = 0; i < 300; i++)
        sum = sum * 31 + b[i];
    printf("overlap flags %lx sum %08x b[100] %d\n", f, sum, b[100]);
    f = copy(b + 1, b, 1, 0);
    printf("one flags %lx b[1] %d\n", f, b[1]);
    return 0;
}
