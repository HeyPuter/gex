/* soundness [linux]: a signal raised by a raw kill() inside nested calls, whose handler reads
 * the interrupted registers from its ucontext. An asm holder loads known values into
 * callee-saved registers, which are live after the call, and calls a leaf that signals its
 * own process. A function-tier caller keeps them in locals across the call (landing pads,
 * gex docs/unwind.md), so the handler's ucontext must still show them exactly, and the holder
 * must find them intact once the handler returns. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <ucontext.h>

void holder(uint64_t *out);
#if defined(__x86_64__)
__asm__(".text\n.globl kill_leaf\n.type kill_leaf,@function\nkill_leaf:\n"
        "mov $39, %eax\n syscall\n mov %eax, %edi\n mov $10, %esi\n mov $62, %eax\n syscall\n ret\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "push %rbx\n push %r12\n push %r13\n push %r14\n push %rdi\n"
        "movabs $0x1b1b1b1b1b1b1b1b, %rbx\n movabs $0x1c1c1c1c1c1c1c1c, %r12\n"
        "movabs $0x1d1d1d1d1d1d1d1d, %r13\n movabs $0x1e1e1e1e1e1e1e1e, %r14\n"
        "call kill_leaf\n"
        "pop %rdi\n mov %rbx, (%rdi)\n mov %r12, 8(%rdi)\n mov %r13, 16(%rdi)\n mov %r14, 24(%rdi)\n"
        "pop %r14\n pop %r13\n pop %r12\n pop %rbx\n ret\n");
static const uint64_t K[4] = {0x1b1b1b1b1b1b1b1bull, 0x1c1c1c1c1c1c1c1cull, 0x1d1d1d1d1d1d1d1dull, 0x1e1e1e1e1e1e1e1eull};
static void seen_regs(const ucontext_t *uc, uint64_t *r) {
    r[0] = (uint64_t)uc->uc_mcontext.gregs[REG_RBX];
    r[1] = (uint64_t)uc->uc_mcontext.gregs[REG_R12];
    r[2] = (uint64_t)uc->uc_mcontext.gregs[REG_R13];
    r[3] = (uint64_t)uc->uc_mcontext.gregs[REG_R14];
}
#elif defined(__aarch64__)
__asm__(".text\n.globl kill_leaf\n.type kill_leaf,%function\nkill_leaf:\n"
        "mov x8, #172\n svc #0\n mov x1, #10\n mov x8, #129\n svc #0\n ret\n"
        ".globl holder\n.type holder,%function\nholder:\n"
        "stp x29, x30, [sp, #-48]!\n mov x29, sp\n stp x19, x20, [sp, #16]\n stp x21, x22, [sp, #32]\n"
        "mov x22, x0\n"
        "mov x19, #0x1919\n movk x19, #0xf19f, lsl #48\n mov x20, #0x2020\n movk x20, #0xf20f, lsl #48\n"
        "mov x21, #0x2121\n movk x21, #0xf21f, lsl #48\n"
        "bl kill_leaf\n"
        "stp x19, x20, [x22]\n str x21, [x22, #16]\n mov x9, #0x2222\n str x9, [x22, #24]\n"
        "ldp x19, x20, [sp, #16]\n ldp x21, x22, [sp, #32]\n ldp x29, x30, [sp], #48\n ret\n");
static const uint64_t K[4] = {0xf19f000000001919ull, 0xf20f000000002020ull, 0xf21f000000002121ull, 0x2222};
static void seen_regs(const ucontext_t *uc, uint64_t *r) {
    r[0] = uc->uc_mcontext.regs[19];
    r[1] = uc->uc_mcontext.regs[20];
    r[2] = uc->uc_mcontext.regs[21];
    r[3] = 0x2222; /* x22 holds `out`, which the handler cannot know */
}
#elif defined(__i386__)
__asm__(".text\n.globl kill_leaf\n.type kill_leaf,@function\nkill_leaf:\n"
        "push %ebx\n mov $20, %eax\n int $0x80\n mov %eax, %ebx\n mov $10, %ecx\n mov $37, %eax\n int $0x80\n pop %ebx\n ret\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "push %ebx\n push %esi\n push %edi\n push %ebp\n"
        "mov $0x1b1b1b1b, %ebx\n mov $0x1c1c1c1c, %esi\n mov $0x1d1d1d1d, %edi\n mov $0x1e1e1e1e, %ebp\n"
        "call kill_leaf\n"
        "mov 20(%esp), %ecx\n"
        "mov %ebx, (%ecx)\n movl $0, 4(%ecx)\n mov %esi, 8(%ecx)\n movl $0, 12(%ecx)\n"
        "mov %edi, 16(%ecx)\n movl $0, 20(%ecx)\n mov %ebp, 24(%ecx)\n movl $0, 28(%ecx)\n"
        "pop %ebp\n pop %edi\n pop %esi\n pop %ebx\n ret\n");
static const uint64_t K[4] = {0x1b1b1b1b, 0x1c1c1c1c, 0x1d1d1d1d, 0x1e1e1e1e};
static void seen_regs(const ucontext_t *uc, uint64_t *r) {
    r[0] = 0x1b1b1b1b; /* ebx carries kill's first argument at the syscall */
    r[1] = (uint32_t)uc->uc_mcontext.gregs[REG_ESI];
    r[2] = (uint32_t)uc->uc_mcontext.gregs[REG_EDI];
    r[3] = (uint32_t)uc->uc_mcontext.gregs[REG_EBP];
}
#elif defined(__powerpc__) && !defined(__powerpc64__)
/* r14-r17 are callee-saved (32-bit); out[] gets them zero-extended, big-endian. */
__asm__(".text\n.globl kill_leaf\n.type kill_leaf,@function\nkill_leaf:\n"
        "li 0, 20\n sc\n li 4, 10\n li 0, 37\n sc\n blr\n"
        ".globl holder\n.type holder,@function\nholder:\n"
        "stwu 1, -48(1)\n mflr 0\n stw 0, 52(1)\n stw 14, 8(1)\n stw 15, 12(1)\n stw 16, 16(1)\n stw 17, 20(1)\n"
        "stw 3, 24(1)\n"
        "lis 14, 0x1b1b\n ori 14, 14, 0x1b1b\n lis 15, 0x1c1c\n ori 15, 15, 0x1c1c\n"
        "lis 16, 0x1d1d\n ori 16, 16, 0x1d1d\n lis 17, 0x1e1e\n ori 17, 17, 0x1e1e\n"
        "bl kill_leaf\n lwz 9, 24(1)\n"
        "li 0, 0\n stw 0, 0(9)\n stw 14, 4(9)\n stw 0, 8(9)\n stw 15, 12(9)\n stw 0, 16(9)\n stw 16, 20(9)\n stw 0, 24(9)\n stw 17, 28(9)\n"
        "lwz 14, 8(1)\n lwz 15, 12(1)\n lwz 16, 16(1)\n lwz 17, 20(1)\n lwz 0, 52(1)\n mtlr 0\n addi 1, 1, 48\n blr\n");
static const uint64_t K[4] = {0x1b1b1b1b, 0x1c1c1c1c, 0x1d1d1d1d, 0x1e1e1e1e};
static void seen_regs(const ucontext_t *uc, uint64_t *r) {
    for (int i = 0; i < 4; ++i) r[i] = (uint32_t)uc->uc_mcontext.gregs[14 + i];
}
#else
#error "sig_frames: no asm for this architecture"
#endif

static volatile sig_atomic_t hits;
static uint64_t in_handler[4];

static void handler(int sig, siginfo_t *si, void *ctx) {
    (void)sig;
    (void)si;
    hits++;
    seen_regs((const ucontext_t *)ctx, in_handler);
}

static int same(const uint64_t *r) {
    int n = 0;
    for (int i = 0; i < 4; ++i) n += r[i] == K[i];
    return n;
}

/* C frames between main and the holder, each with values live across its call. */
__attribute__((noinline)) static uint64_t nest(int n, uint64_t s, uint64_t *out) {
    uint64_t a = s * 3 + 1, b = s ^ 0x5a5a, c = s + 7, d = s * s;
    uint64_t r = n == 0 ? (holder(out), 0) : nest(n - 1, s * 5 + 1, out);
    return r * 31 + a + b * 3 + c * 5 + d * 7;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGUSR1, &sa, 0);
    static volatile uint64_t seed = 99;
    for (int round = 0; round < 3; ++round) {
        uint64_t out[4] = {0};
        memset(in_handler, 0, sizeof in_handler);
        uint64_t v = nest(round * 2, seed + round, out);
        printf("round %d: hits %d, handler saw %d/4 held registers, holder kept %d/4, nest %llx\n", round,
               (int)hits, same(in_handler), same(out), (unsigned long long)v);
    }
    return 0;
}
