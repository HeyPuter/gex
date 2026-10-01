/* soundness [linux threads]: fork from inside nested calls. An asm holder loads known values into
 * callee-saved registers and calls a leaf that forks with a raw syscall, so a function-tier
 * caller keeps them in locals across the call (landing pads, gex docs/unwind.md). Child and
 * parent must both see the values intact after the fork, and fork's return values must be
 * right: 0 in the child, the child's pid in the parent. A second round forks through libc
 * from C frames that keep many values live across the call. */
#include <stdint.h>
#include <stdio.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

/* holder(out): callee-saved registers := K0..K3, fork_leaf(), out[0..3] := those registers,
 * out[4] := fork's result. */
void holder(uint64_t *out);
#if defined(__x86_64__)
__asm__(".text\n.globl fork_leaf\n.type fork_leaf,@function\nfork_leaf:\n"
        "mov $57, %eax\n syscall\n ret\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "push %rbx\n push %r12\n push %r13\n push %r14\n push %rdi\n"
        "movabs $0x1b1b1b1b1b1b1b1b, %rbx\n movabs $0x1c1c1c1c1c1c1c1c, %r12\n"
        "movabs $0x1d1d1d1d1d1d1d1d, %r13\n movabs $0x1e1e1e1e1e1e1e1e, %r14\n"
        "call fork_leaf\n"
        "pop %rdi\n mov %rbx, (%rdi)\n mov %r12, 8(%rdi)\n mov %r13, 16(%rdi)\n mov %r14, 24(%rdi)\n mov %rax, 32(%rdi)\n"
        "pop %r14\n pop %r13\n pop %r12\n pop %rbx\n ret\n");
#define K(i) (0x1b1b1b1b1b1b1b1bull + 0x0101010101010101ull * (i))
#elif defined(__aarch64__)
__asm__(".text\n.globl fork_leaf\n.type fork_leaf,%function\nfork_leaf:\n"
        "mov x0, #17\n mov x1, #0\n mov x2, #0\n mov x3, #0\n mov x4, #0\n mov x8, #220\n svc #0\n ret\n"
        ".globl holder\n.type holder,%function\nholder:\n"
        "stp x29, x30, [sp, #-48]!\n mov x29, sp\n stp x19, x20, [sp, #16]\n stp x21, x22, [sp, #32]\n"
        "mov x22, x0\n"
        "mov x19, #0x1919\n movk x19, #0xf19f, lsl #48\n mov x20, #0x2020\n movk x20, #0xf20f, lsl #48\n"
        "mov x21, #0x2121\n movk x21, #0xf21f, lsl #48\n"
        "bl fork_leaf\n"
        "stp x19, x20, [x22]\n str x21, [x22, #16]\n str x22, [x22, #24]\n str x0, [x22, #32]\n"
        "ldp x19, x20, [sp, #16]\n ldp x21, x22, [sp, #32]\n ldp x29, x30, [sp], #48\n ret\n");
#define K(i) ((i) == 0 ? 0xf19f000000001919ull : (i) == 1 ? 0xf20f000000002020ull : (i) == 2 ? 0xf21f000000002121ull : (uint64_t)(uintptr_t)out)
#elif defined(__i386__)
__asm__(".text\n.globl fork_leaf\n.type fork_leaf,@function\nfork_leaf:\n"
        "mov $2, %eax\n int $0x80\n ret\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "push %ebx\n push %esi\n push %edi\n push %ebp\n"
        "mov $0x1b1b1b1b, %ebx\n mov $0x1c1c1c1c, %esi\n mov $0x1d1d1d1d, %edi\n mov $0x1e1e1e1e, %ebp\n"
        "call fork_leaf\n"
        "mov 20(%esp), %ecx\n"
        "mov %ebx, (%ecx)\n movl $0, 4(%ecx)\n mov %esi, 8(%ecx)\n movl $0, 12(%ecx)\n"
        "mov %edi, 16(%ecx)\n movl $0, 20(%ecx)\n mov %ebp, 24(%ecx)\n movl $0, 28(%ecx)\n"
        "mov %eax, 32(%ecx)\n sar $31, %eax\n mov %eax, 36(%ecx)\n"
        "pop %ebp\n pop %edi\n pop %esi\n pop %ebx\n ret\n");
#define K(i) (0x1b1b1b1bull + 0x01010101ull * (i))
#elif defined(__powerpc__) && !defined(__powerpc64__)
__asm__(".text\n.globl fork_leaf\n.type fork_leaf,@function\nfork_leaf:\n"
        "li 0, 2\n sc\n bns 1f\n neg 3, 3\n1: blr\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "stwu 1, -48(1)\n mflr 0\n stw 0, 52(1)\n stw 14, 8(1)\n stw 15, 12(1)\n stw 16, 16(1)\n stw 17, 20(1)\n"
        "stw 3, 24(1)\n"
        "lis 14, 0x1b1b\n ori 14, 14, 0x1b1b\n lis 15, 0x1c1c\n ori 15, 15, 0x1c1c\n"
        "lis 16, 0x1d1d\n ori 16, 16, 0x1d1d\n lis 17, 0x1e1e\n ori 17, 17, 0x1e1e\n"
        "bl fork_leaf\n lwz 9, 24(1)\n"
        "li 0, 0\n stw 0, 0(9)\n stw 14, 4(9)\n stw 0, 8(9)\n stw 15, 12(9)\n stw 0, 16(9)\n stw 16, 20(9)\n stw 0, 24(9)\n stw 17, 28(9)\n"
        "srawi 0, 3, 31\n stw 0, 32(9)\n stw 3, 36(9)\n"
        "lwz 14, 8(1)\n lwz 15, 12(1)\n lwz 16, 16(1)\n lwz 17, 20(1)\n lwz 0, 52(1)\n mtlr 0\n addi 1, 1, 48\n blr\n");
#define K(i) ((i) == 0 ? 0x1b1b1b1bull : (i) == 1 ? 0x1c1c1c1cull : (i) == 2 ? 0x1d1d1d1dull : 0x1e1e1e1eull)
#else
#error "fork_frames: no asm for this architecture"
#endif

static int check(const uint64_t *out) {
    int ok = 1;
    for (int i = 0; i < 4; ++i) ok &= out[i] == K(i);
    return ok;
}

/* A C frame between main and the holder, with values live across the call. */
__attribute__((noinline)) static int middle(uint64_t *out, volatile uint64_t *seed) {
    uint64_t a = *seed * 3 + 1, b = *seed ^ 0x55, c = *seed + 7, d = *seed * *seed;
    holder(out);
    return (int)((a + b * 3 + c * 5 + d * 7) & 0xffff);
}

/* The libc round: fork() several frames down, values live in each frame. */
__attribute__((noinline)) static uint64_t deep_fork(int n, uint64_t s, pid_t *who) {
    uint64_t a = s * 3 + 1, b = s ^ 0x5a5a, c = s + 7, d = s * s, e = a ^ c, f = b + d;
    uint64_t r;
    if (n == 0) {
        fflush(stdout);
        *who = fork();
        r = 0;
    } else {
        r = deep_fork(n - 1, s * 7 + 3, who);
    }
    return r * 31 + a + b * 3 + c * 5 + d * 7 + e * 11 + f * 13;
}

int main(void) {
    static volatile uint64_t seed = 12345;
    uint64_t out[5] = {0};
    fflush(stdout);
    int m = middle(out, &seed);
    int64_t ret = (int64_t)out[4];
    if (ret == 0) {
        printf("asm child: regs %s, middle %d\n", check(out) ? "intact" : "CLOBBERED", m);
        fflush(stdout);
        _exit(0);
    }
    int status = -1;
    pid_t w = waitpid((pid_t)ret, &status, 0);
    printf("asm parent: regs %s, middle %d, fork returned the child: %d, child exit %d\n",
           check(out) ? "intact" : "CLOBBERED", m, w == (pid_t)ret && ret > 0,
           WIFEXITED(status) ? WEXITSTATUS(status) : -1);

    pid_t who = -1;
    uint64_t v = deep_fork(6, seed, &who);
    if (who == 0) {
        printf("libc child: %llx\n", (unsigned long long)v);
        fflush(stdout);
        _exit(3);
    }
    status = -1;
    w = waitpid(who, &status, 0);
    printf("libc parent: %llx, fork returned the child: %d, child exit %d\n", (unsigned long long)v,
           w == who && who > 0, WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    return 0;
}
