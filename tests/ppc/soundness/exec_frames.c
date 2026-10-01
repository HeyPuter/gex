/* soundness [linux]: execve from inside nested calls. An asm holder loads known values into
 * callee-saved registers and calls a leaf that makes a raw execve, so a function-tier caller
 * keeps them in locals across the call (landing pads, gex docs/unwind.md). A failing execve
 * must return -ENOENT with the registers intact; a successful one (of /proc/self/exe) must
 * start the new image cleanly, whatever the frames it replaced were holding. */
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* holder(path, argv, envp, out): callee-saved registers := K0..K3, exec_leaf(path, argv,
 * envp), out[0..3] := those registers, out[4] := the result. */
void holder(const char *path, char *const *argv, char *const *envp, uint64_t *out);
#if defined(__x86_64__)
__asm__(".text\n.globl exec_leaf\n.type exec_leaf,@function\nexec_leaf:\n"
        "mov $59, %eax\n syscall\n ret\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "push %rbx\n push %r12\n push %r13\n push %r14\n push %rcx\n"
        "movabs $0x1b1b1b1b1b1b1b1b, %rbx\n movabs $0x1c1c1c1c1c1c1c1c, %r12\n"
        "movabs $0x1d1d1d1d1d1d1d1d, %r13\n movabs $0x1e1e1e1e1e1e1e1e, %r14\n"
        "call exec_leaf\n"
        "pop %rcx\n mov %rbx, (%rcx)\n mov %r12, 8(%rcx)\n mov %r13, 16(%rcx)\n mov %r14, 24(%rcx)\n mov %rax, 32(%rcx)\n"
        "pop %r14\n pop %r13\n pop %r12\n pop %rbx\n ret\n");
static const uint64_t K[4] = {0x1b1b1b1b1b1b1b1bull, 0x1c1c1c1c1c1c1c1cull, 0x1d1d1d1d1d1d1d1dull, 0x1e1e1e1e1e1e1e1eull};
#elif defined(__aarch64__)
__asm__(".text\n.globl exec_leaf\n.type exec_leaf,%function\nexec_leaf:\n"
        "mov x8, #221\n svc #0\n ret\n"
        ".globl holder\n.type holder,%function\nholder:\n"
        "stp x29, x30, [sp, #-48]!\n mov x29, sp\n stp x19, x20, [sp, #16]\n stp x21, x22, [sp, #32]\n"
        "mov x22, x3\n"
        "mov x19, #0x1919\n movk x19, #0xf19f, lsl #48\n mov x20, #0x2020\n movk x20, #0xf20f, lsl #48\n"
        "mov x21, #0x2121\n movk x21, #0xf21f, lsl #48\n"
        "bl exec_leaf\n"
        "stp x19, x20, [x22]\n str x21, [x22, #16]\n mov x9, #0x2222\n str x9, [x22, #24]\n str x0, [x22, #32]\n"
        "ldp x19, x20, [sp, #16]\n ldp x21, x22, [sp, #32]\n ldp x29, x30, [sp], #48\n ret\n");
static const uint64_t K[4] = {0xf19f000000001919ull, 0xf20f000000002020ull, 0xf21f000000002121ull, 0x2222};
#elif defined(__i386__)
__asm__(".text\n.globl exec_leaf\n.type exec_leaf,@function\nexec_leaf:\n"
        "push %ebx\n mov 8(%esp), %ebx\n mov 12(%esp), %ecx\n mov 16(%esp), %edx\n mov $11, %eax\n int $0x80\n pop %ebx\n ret\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "push %ebx\n push %esi\n push %edi\n push %ebp\n"
        "mov $0x1b1b1b1b, %ebx\n mov $0x1c1c1c1c, %esi\n mov $0x1d1d1d1d, %edi\n mov $0x1e1e1e1e, %ebp\n"
        "push 28(%esp)\n push 28(%esp)\n push 28(%esp)\n call exec_leaf\n add $12, %esp\n"
        "mov 32(%esp), %ecx\n"
        "mov %ebx, (%ecx)\n movl $0, 4(%ecx)\n mov %esi, 8(%ecx)\n movl $0, 12(%ecx)\n"
        "mov %edi, 16(%ecx)\n movl $0, 20(%ecx)\n mov %ebp, 24(%ecx)\n movl $0, 28(%ecx)\n"
        "mov %eax, 32(%ecx)\n sar $31, %eax\n mov %eax, 36(%ecx)\n"
        "pop %ebp\n pop %edi\n pop %esi\n pop %ebx\n ret\n");
static const uint64_t K[4] = {0x1b1b1b1b, 0x1c1c1c1c, 0x1d1d1d1d, 0x1e1e1e1e};
#elif defined(__powerpc__) && !defined(__powerpc64__)
__asm__(".text\n.globl exec_leaf\n.type exec_leaf,@function\nexec_leaf:\n"
        "li 0, 11\n sc\n bns 1f\n neg 3, 3\n1: blr\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "stwu 1, -48(1)\n mflr 0\n stw 0, 52(1)\n stw 14, 8(1)\n stw 15, 12(1)\n stw 16, 16(1)\n stw 17, 20(1)\n"
        "stw 6, 24(1)\n"
        "lis 14, 0x1b1b\n ori 14, 14, 0x1b1b\n lis 15, 0x1c1c\n ori 15, 15, 0x1c1c\n"
        "lis 16, 0x1d1d\n ori 16, 16, 0x1d1d\n lis 17, 0x1e1e\n ori 17, 17, 0x1e1e\n"
        "bl exec_leaf\n lwz 9, 24(1)\n"
        "li 0, 0\n stw 0, 0(9)\n stw 14, 4(9)\n stw 0, 8(9)\n stw 15, 12(9)\n stw 0, 16(9)\n stw 16, 20(9)\n stw 0, 24(9)\n stw 17, 28(9)\n"
        "srawi 0, 3, 31\n stw 0, 32(9)\n stw 3, 36(9)\n"
        "lwz 14, 8(1)\n lwz 15, 12(1)\n lwz 16, 16(1)\n lwz 17, 20(1)\n lwz 0, 52(1)\n mtlr 0\n addi 1, 1, 48\n blr\n");
static const uint64_t K[4] = {0x1b1b1b1b, 0x1c1c1c1c, 0x1d1d1d1d, 0x1e1e1e1e};
#else
#error "exec_frames: no asm for this architecture"
#endif

extern char **environ;

static int same(const uint64_t *r) {
    int n = 0;
    for (int i = 0; i < 4; ++i) n += r[i] == K[i];
    return n;
}

__attribute__((noinline)) static uint64_t nest(int n, uint64_t s, const char *path, char *const *argv, uint64_t *out) {
    uint64_t a = s * 3 + 1, b = s ^ 0x5a5a, c = s + 7, d = s * s;
    uint64_t r = n == 0 ? (holder(path, argv, environ, out), 0) : nest(n - 1, s * 5 + 1, path, argv, out);
    return r * 31 + a + b * 3 + c * 5 + d * 7;
}

int main(int argc, char **argv) {
    if (argc > 2 && strcmp(argv[1], "exec-child") == 0) {
        printf("new image: %s, %d arguments\n", argv[2], argc);
        return 0;
    }
    static volatile uint64_t seed = 7;
    uint64_t out[5] = {0};
    char *bad[] = {"nowhere", 0};
    uint64_t v = nest(4, seed, "/nonexistent/exec_frames", bad, out);
    printf("failed exec: ENOENT %d, holder kept %d/4, nest %llx\n", (int64_t)out[4] == -ENOENT, same(out),
           (unsigned long long)v);
    fflush(stdout);
    char *args[] = {"exec_frames", "exec-child", "token-42", 0};
    memset(out, 0, sizeof out);
    nest(5, seed, "/proc/self/exe", args, out);
    printf("exec of /proc/self/exe returned %lld\n", (long long)(int64_t)out[4]);
    return 1;
}
