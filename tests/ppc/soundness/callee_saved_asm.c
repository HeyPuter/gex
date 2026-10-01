/* soundness []: callee-saved registers deliberately clobbered and restored in per-arch asm.
 * clobber_call(fn, arg) saves every callee-saved register, loads garbage into all of them, calls
 * the C function fn (which must preserve them), checks the garbage survived, restores the
 * originals and returns fn(arg) -- or -1 if the callee broke the convention. The C caller keeps
 * many values live across the call, so they sit in the registers being clobbered. */
#include <stdio.h>
#include <stdint.h>
int32_t clobber_call(int32_t (*fn)(int32_t), int32_t arg);
#if defined(_WIN32) && defined(__i386__)
#define SYM "_clobber_call"
#else
#define SYM "clobber_call"
#endif
#if defined(__x86_64__)
__asm__(".text\n.globl " SYM "\n" SYM ":\n"
        "push %rbx\n push %rbp\n push %r12\n push %r13\n push %r14\n push %r15\n sub $8, %rsp\n"
        "movabs $0x1111111111111111, %rbx\n movabs $0x2222222222222222, %rbp\n movabs $0x3333333333333333, %r12\n"
        "movabs $0x4444444444444444, %r13\n movabs $0x5555555555555555, %r14\n movabs $0x6666666666666666, %r15\n"
        "mov %rdi, %rax\n mov %esi, %edi\n call *%rax\n"
        "movabs $0x1111111111111111, %rcx\n cmp %rcx, %rbx\n jne 1f\n movabs $0x2222222222222222, %rcx\n cmp %rcx, %rbp\n jne 1f\n"
        "movabs $0x3333333333333333, %rcx\n cmp %rcx, %r12\n jne 1f\n movabs $0x4444444444444444, %rcx\n cmp %rcx, %r13\n jne 1f\n"
        "movabs $0x5555555555555555, %rcx\n cmp %rcx, %r14\n jne 1f\n movabs $0x6666666666666666, %rcx\n cmp %rcx, %r15\n jne 1f\n"
        "jmp 2f\n1: mov $-1, %eax\n2:\n"
        "add $8, %rsp\n pop %r15\n pop %r14\n pop %r13\n pop %r12\n pop %rbp\n pop %rbx\n ret\n");
#elif defined(__aarch64__)
__asm__(".text\n.globl " SYM "\n" SYM ":\n"
        "stp x29, x30, [sp, #-96]!\n mov x29, sp\n"
        "stp x19, x20, [sp, #16]\n stp x21, x22, [sp, #32]\n stp x23, x24, [sp, #48]\n stp x25, x26, [sp, #64]\n stp x27, x28, [sp, #80]\n"
        "mov x19, #0x1111\n mov x20, #0x2222\n mov x21, #0x3333\n mov x22, #0x4444\n mov x23, #0x5555\n"
        "mov x24, #0x6666\n mov x25, #0x7777\n mov x26, #0x8888\n mov x27, #0x9999\n mov x28, #0xaaaa\n"
        "movk x19, #0xf00d, lsl #48\n movk x28, #0xbeef, lsl #32\n"
        "mov x9, x0\n mov w0, w1\n blr x9\n"
        "mov x9, #0x1111\n movk x9, #0xf00d, lsl #48\n eor x10, x19, x9\n"
        "mov x9, #0x2222\n eor x9, x20, x9\n orr x10, x10, x9\n mov x9, #0x3333\n eor x9, x21, x9\n orr x10, x10, x9\n"
        "mov x9, #0x4444\n eor x9, x22, x9\n orr x10, x10, x9\n mov x9, #0x5555\n eor x9, x23, x9\n orr x10, x10, x9\n"
        "mov x9, #0x6666\n eor x9, x24, x9\n orr x10, x10, x9\n mov x9, #0x7777\n eor x9, x25, x9\n orr x10, x10, x9\n"
        "mov x9, #0x8888\n eor x9, x26, x9\n orr x10, x10, x9\n mov x9, #0x9999\n eor x9, x27, x9\n orr x10, x10, x9\n"
        "mov x9, #0xaaaa\n movk x9, #0xbeef, lsl #32\n eor x9, x28, x9\n orr x10, x10, x9\n"
        "mov w9, #-1\n cmp x10, #0\n csel w0, w0, w9, eq\n"
        "ldp x19, x20, [sp, #16]\n ldp x21, x22, [sp, #32]\n ldp x23, x24, [sp, #48]\n ldp x25, x26, [sp, #64]\n ldp x27, x28, [sp, #80]\n"
        "ldp x29, x30, [sp], #96\n ret\n");
#elif defined(__i386__)
__asm__(".text\n.globl " SYM "\n" SYM ":\n"
        "push %ebp\n push %ebx\n push %esi\n push %edi\n"
        "mov 20(%esp), %eax\n mov 24(%esp), %ecx\n"
        "mov $0x11111111, %ebx\n mov $0x22222222, %esi\n mov $0x33333333, %edi\n mov $0x44444444, %ebp\n"
        "sub $8, %esp\n push %ecx\n call *%eax\n add $12, %esp\n"
        "cmp $0x11111111, %ebx\n jne 1f\n cmp $0x22222222, %esi\n jne 1f\n cmp $0x33333333, %edi\n jne 1f\n cmp $0x44444444, %ebp\n jne 1f\n"
        "jmp 2f\n1: mov $-1, %eax\n2:\n"
        "pop %edi\n pop %esi\n pop %ebx\n pop %ebp\n ret\n");
#elif defined(__powerpc__) && !defined(__powerpc64__)
/* r14-r31 are callee-saved; r3 = fn, r4 = arg. */
__asm__(".text\n.globl " SYM "\n" SYM ":\n"
        "stwu 1, -96(1)\n mflr 0\n stw 0, 100(1)\n stmw 14, 24(1)\n"
        "lis 14, 0x5a0e\n ori 14, 14, 0x0ea5\n lis 15, 0x5a0f\n ori 15, 15, 0x0fa5\n lis 16, 0x5a10\n ori 16, 16, 0x10a5\n lis 17, 0x5a11\n ori 17, 17, 0x11a5\n lis 18, 0x5a12\n ori 18, 18, 0x12a5\n lis 19, 0x5a13\n ori 19, 19, 0x13a5\n lis 20, 0x5a14\n ori 20, 20, 0x14a5\n lis 21, 0x5a15\n ori 21, 21, 0x15a5\n lis 22, 0x5a16\n ori 22, 22, 0x16a5\n lis 23, 0x5a17\n ori 23, 23, 0x17a5\n lis 24, 0x5a18\n ori 24, 24, 0x18a5\n lis 25, 0x5a19\n ori 25, 25, 0x19a5\n lis 26, 0x5a1a\n ori 26, 26, 0x1aa5\n lis 27, 0x5a1b\n ori 27, 27, 0x1ba5\n lis 28, 0x5a1c\n ori 28, 28, 0x1ca5\n lis 29, 0x5a1d\n ori 29, 29, 0x1da5\n lis 30, 0x5a1e\n ori 30, 30, 0x1ea5\n lis 31, 0x5a1f\n ori 31, 31, 0x1fa5\n "
        "mtctr 3\n mr 3, 4\n bctrl\n li 10, 0\n"
        "lis 12, 0x5a0e\n ori 12, 12, 0x0ea5\n xor 11, 14, 12\n or 10, 10, 11\n lis 12, 0x5a0f\n ori 12, 12, 0x0fa5\n xor 11, 15, 12\n or 10, 10, 11\n lis 12, 0x5a10\n ori 12, 12, 0x10a5\n xor 11, 16, 12\n or 10, 10, 11\n lis 12, 0x5a11\n ori 12, 12, 0x11a5\n xor 11, 17, 12\n or 10, 10, 11\n lis 12, 0x5a12\n ori 12, 12, 0x12a5\n xor 11, 18, 12\n or 10, 10, 11\n lis 12, 0x5a13\n ori 12, 12, 0x13a5\n xor 11, 19, 12\n or 10, 10, 11\n lis 12, 0x5a14\n ori 12, 12, 0x14a5\n xor 11, 20, 12\n or 10, 10, 11\n lis 12, 0x5a15\n ori 12, 12, 0x15a5\n xor 11, 21, 12\n or 10, 10, 11\n lis 12, 0x5a16\n ori 12, 12, 0x16a5\n xor 11, 22, 12\n or 10, 10, 11\n lis 12, 0x5a17\n ori 12, 12, 0x17a5\n xor 11, 23, 12\n or 10, 10, 11\n lis 12, 0x5a18\n ori 12, 12, 0x18a5\n xor 11, 24, 12\n or 10, 10, 11\n lis 12, 0x5a19\n ori 12, 12, 0x19a5\n xor 11, 25, 12\n or 10, 10, 11\n lis 12, 0x5a1a\n ori 12, 12, 0x1aa5\n xor 11, 26, 12\n or 10, 10, 11\n lis 12, 0x5a1b\n ori 12, 12, 0x1ba5\n xor 11, 27, 12\n or 10, 10, 11\n lis 12, 0x5a1c\n ori 12, 12, 0x1ca5\n xor 11, 28, 12\n or 10, 10, 11\n lis 12, 0x5a1d\n ori 12, 12, 0x1da5\n xor 11, 29, 12\n or 10, 10, 11\n lis 12, 0x5a1e\n ori 12, 12, 0x1ea5\n xor 11, 30, 12\n or 10, 10, 11\n lis 12, 0x5a1f\n ori 12, 12, 0x1fa5\n xor 11, 31, 12\n or 10, 10, 11\n "
        "cmpwi 10, 0\n beq 1f\n li 3, -1\n1:\n lmw 14, 24(1)\n lwz 0, 100(1)\n mtlr 0\n addi 1, 1, 96\n blr\n");
#else
#error "callee_saved_asm: no asm for this architecture"
#endif
/* Uses enough registers that the compiler must save and restore callee-saved ones itself. */
__attribute__((noinline)) static int32_t busy(int32_t x) {
    uint32_t a = (uint32_t)x, b = a * 3, c = a ^ 0x55, d = a + 7, e = b * c, f = d ^ e, g = a * a, h = f + g;
    for (int i = 0; i < 10; ++i) { a += h; b ^= a; c += b * 3; d -= c; e ^= d; f += e; g ^= f; h += g * 5; }
    return (int32_t)((a ^ b ^ c ^ d ^ e ^ f ^ g ^ h) & 0x7fffffff);
}
__attribute__((noinline)) static int32_t nested(int32_t x) { return clobber_call(busy, x + 1) ^ 0x1234; }
int main(void) {
    volatile int32_t seed = 9;
    int32_t v0 = seed, v1 = v0 * 3, v2 = v1 + 11, v3 = v2 ^ 0x77, v4 = v3 * 5, v5 = v4 - 2, v6 = v5 ^ v0, v7 = v6 + v1;
    int32_t r1 = clobber_call(busy, v0);
    int32_t r2 = clobber_call(nested, v7);  /* a C function that itself calls back into the asm */
    printf("r1=%d r2=%d\n", r1, r2);
    printf("live=%d %d %d %d %d %d %d %d\n", v0, v1, v2, v3, v4, v5, v6, v7);
    return 0;
}
