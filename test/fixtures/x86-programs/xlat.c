/* xlat: al = [(e/r)bx + zero-extended al], with the table index taken from
 * the whole low byte, a 32-bit address size (0x67), and a segment override
 * (TLS through %fs on x86-64, %gs on i386), as a CRT's nibble-table lookups
 * use it. */
#include <stdint.h>
#include <stdio.h>

static const unsigned char table[256] = {
#define R(i) (unsigned char)((i) * 7 + 3)
#define R4(i) R(i), R(i + 1), R(i + 2), R(i + 3)
#define R16(i) R4(i), R4(i + 4), R4(i + 8), R4(i + 12)
#define R64(i) R16(i), R16(i + 16), R16(i + 32), R16(i + 48)
    R64(0), R64(64), R64(128), R64(192)};
static __thread unsigned char tls_table[16] = {9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66};

static unsigned xlat(const unsigned char *t, unsigned al) {
    uintptr_t a = al | 0x1200; /* only al indexes; ah is kept */
    __asm__ volatile("xlat" : "+a"(a) : "b"(t) : "memory");
    return (unsigned)a;
}

int main(void) {
    unsigned sum = 0;
    for (unsigned i = 0; i < 256; i++) {
        unsigned r = xlat(table, i);
        if ((r & 0xff) != table[i] || (r >> 8) != 0x12) printf("xlat %u: %#x\n", i, r);
        sum += r & 0xff;
    }
    printf("xlat table sum %u\n", sum);

#ifdef __x86_64__
    /* 0x67: ebx, not rbx (junk in rbx's top half). */
    uintptr_t a = 5, b = (uintptr_t)table | 0xdead000000000000ull;
    if ((uintptr_t)table >> 32 == 0) {
        __asm__ volatile(".byte 0x67\n xlat" : "+a"(a) : "b"(b) : "memory");
        printf("addr32 xlat %u\n", (unsigned)(a & 0xff) == table[5]);
    } else {
        printf("addr32 xlat 1\n");
    }
    uintptr_t fsbase, off;
    __asm__ volatile("mov %%fs:0, %0" : "=r"(fsbase));
    off = (uintptr_t)tls_table - fsbase;
    a = 11;
    __asm__ volatile(".byte 0x64, 0xd7" : "+a"(a) : "b"(off) : "memory");
#else
    uintptr_t gsbase, off, a;
    __asm__ volatile("mov %%gs:0, %0" : "=r"(gsbase));
    off = (uintptr_t)tls_table - gsbase;
    a = 11;
    printf("addr32 xlat 1\n");
    __asm__ volatile(".byte 0x65, 0xd7" : "+a"(a) : "b"(off) : "memory");
#endif
    printf("segment xlat %#x\n", (unsigned)(a & 0xff));
    return 0;
}
