/* Self-modifying code reached by a direct jump: a stub copies a function's
 * bytes into another mapping (rep movsb) and jumps there (jmp rel32), as a
 * JIT or an unpacker does. The second time it copies different bytes, so
 * the jump target's block guard sees new code and has it translated again
 * -- at the target, not wherever the last indirect branch went (bass.dll's
 * unpacker jumps to the code it has just written). */
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

static int run(unsigned char *stub, const unsigned char *src, unsigned char *dst) {
    long r;
    long s = (long)src, d = (long)dst, n = 6;
    /* esi/edi/ecx (rsi/rdi/rcx) are the copy's, eax starts at 0: the stub
     * is `rep movsb; call h; add eax, 1; jmp dst` (h: ret), the same bytes
     * in both modes. The call leaves its return address as the last
     * indirect target, which is where the wrong translation went. */
    r = 0;
    __asm__ volatile("call *%[stub]"
                     : "+a"(r), "+S"(s), "+D"(d), "+c"(n)
                     : [stub] "r"(stub)
                     : "edx", "memory", "cc");
    return (int)r;
}

static int other(void) { return 5; }

int main(void) {
    unsigned char *code = mmap((void *)0x30000000, 0x10000, PROT_READ | PROT_WRITE | PROT_EXEC,
                               MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    unsigned char *stub = mmap((void *)0x30100000, 0x10000, PROT_READ | PROT_WRITE | PROT_EXEC,
                               MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    if (code == MAP_FAILED || stub == MAP_FAILED) return 2;
    code += 0x80;
    stub += 0x40;
    /* add eax, N; ret (padded to the copy's six bytes) */
    static const unsigned char one[] = {0x83, 0xc0, 10, 0xc3, 0x90, 0x90};
    static const unsigned char two[] = {0x83, 0xc0, 20, 0xc3, 0x90, 0x90};
    unsigned char *p = stub;
    *p++ = 0xf3; *p++ = 0xa4;                        /* rep movsb */
    int call = 0x20 - 7;                             /* call stub+0x20 */
    *p++ = 0xe8; memcpy(p, &call, 4); p += 4;
    *p++ = 0x83; *p++ = 0xc0; *p++ = 1;              /* add eax, 1 */
    int rel = (int)(code - (p + 5));
    *p++ = 0xe9; memcpy(p, &rel, 4);                 /* jmp code */
    stub[0x20] = 0xc3;                               /* h: ret */
    int a = run(stub, one, code);
    int b = other();
    int c = run(stub, two, code);
    printf("first %d, other %d, rewritten %d\n", a, b, c);
    return 0;
}
