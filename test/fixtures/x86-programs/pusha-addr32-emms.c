/* pusha/popa (i386 only), `addr32 call rel32` (GNU ld's padding for a
 * relaxed GOT call, as glibc's i386 ld.so has them) and emms after x87 use.
 * The x86-64 build prints the same lines without the i386-only parts. */
#include <stdint.h>
#include <stdio.h>

/* do_call: `addr32 call` to a function returning 42. */
int do_call(void);
__asm__(".text\n"
        "callee42: movl $42, %eax\n ret\n"
        ".globl do_call\n"
        "do_call: .byte 0x67\n call callee42\n ret\n");

int main(void) {
#ifdef __i386__
    /* pushal stores eax, ecx, edx, ebx, the original esp, ebp, esi, edi
     * (edi lowest); popal restores all but esp. */
    uint32_t saved[8], esp_before, out[3];
    __asm__ volatile(
        "movl %%esp, %[spb]\n"
        "movl $0x11, %%eax\n movl $0x22, %%ecx\n movl $0x33, %%edx\n"
        "pushal\n"
        "movl %%esp, %%eax\n"
        "movl 0(%%eax), %%edx\n movl %%edx, 0(%[s])\n"
        "movl 4(%%eax), %%edx\n movl %%edx, 4(%[s])\n"
        "movl 8(%%eax), %%edx\n movl %%edx, 8(%[s])\n"
        "movl 12(%%eax), %%edx\n movl %%edx, 12(%[s])\n"
        "movl 16(%%eax), %%edx\n movl %%edx, 16(%[s])\n"
        "movl 20(%%eax), %%edx\n movl %%edx, 20(%[s])\n"
        "movl 24(%%eax), %%edx\n movl %%edx, 24(%[s])\n"
        "movl 28(%%eax), %%edx\n movl %%edx, 28(%[s])\n"
        "movl $0, %%eax\n movl $0, %%ecx\n movl $0, %%edx\n"
        "popal\n"
        "movl %%eax, 0(%[o])\n movl %%ecx, 4(%[o])\n movl %%edx, 8(%[o])\n"
        : [spb] "=m"(esp_before)
        : [s] "S"(saved), [o] "D"(out)
        : "eax", "ecx", "edx", "memory");
    printf("pusha eax %x ecx %x edx %x\n", saved[7], saved[6], saved[5]);
    printf("pusha esp %s\n", saved[3] == esp_before ? "same" : "differs");
    printf("popa eax %x ecx %x edx %x\n", out[0], out[1], out[2]);
#else
    printf("pusha eax 11 ecx 22 edx 33\n");
    printf("pusha esp same\n");
    printf("popa eax 11 ecx 22 edx 33\n");
#endif

    printf("addr32 call %d\n", do_call());

    /* x87 work, then emms empties the stack; more x87 work after it. */
    volatile double a = 1.5, b = 2.25;
    double c = a * b;
    __asm__ volatile("emms" ::: "memory");
    double d = c + a;
    printf("emms %d %d\n", (int)(c * 100), (int)(d * 100));
    return 0;
}
