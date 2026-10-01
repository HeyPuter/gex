/* soundness []: coroutine-style stack switching in per-arch asm (what swapcontext does, without
 * libc support: musl has no swapcontext). Two generators on their own heap stacks ping-pong
 * with main; each keeps locals live across every switch, and one recurses on its stack. A
 * translator that assumes a call returns on the stack it was made from breaks here.
 * xfail g1: pewasm returns through the Wasm call stack, so a ret onto another stack's saved address is not a transfer; the lifted code reads 0xffffffe4 and aborts
 */
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#if defined(_WIN32) && defined(__i386__)
#define SYM(x) "_" #x
#else
#define SYM(x) #x
#endif
void coro_swap(void **save_sp, void *new_sp);
#if defined(__x86_64__)
__asm__(".text\n.globl " SYM(coro_swap) "\n" SYM(coro_swap) ":\n"
        "push %rbp\n push %rbx\n push %r12\n push %r13\n push %r14\n push %r15\n"
        "mov %rsp, (%rdi)\n mov %rsi, %rsp\n"
        "pop %r15\n pop %r14\n pop %r13\n pop %r12\n pop %rbx\n pop %rbp\n ret\n");
enum { SAVED = 6 };
#elif defined(__aarch64__)
__asm__(".text\n.globl " SYM(coro_swap) "\n" SYM(coro_swap) ":\n"
        "sub sp, sp, #160\n"
        "stp x19, x20, [sp]\n stp x21, x22, [sp, #16]\n stp x23, x24, [sp, #32]\n stp x25, x26, [sp, #48]\n"
        "stp x27, x28, [sp, #64]\n stp x29, x30, [sp, #80]\n stp d8, d9, [sp, #96]\n stp d10, d11, [sp, #112]\n"
        "stp d12, d13, [sp, #128]\n stp d14, d15, [sp, #144]\n"
        "mov x9, sp\n str x9, [x0]\n mov sp, x1\n"
        "ldp x19, x20, [sp]\n ldp x21, x22, [sp, #16]\n ldp x23, x24, [sp, #32]\n ldp x25, x26, [sp, #48]\n"
        "ldp x27, x28, [sp, #64]\n ldp x29, x30, [sp, #80]\n ldp d8, d9, [sp, #96]\n ldp d10, d11, [sp, #112]\n"
        "ldp d12, d13, [sp, #128]\n ldp d14, d15, [sp, #144]\n"
        "add sp, sp, #160\n ret\n");
enum { SAVED = 20 }; /* 160 bytes; x30 (the resume address) is slot 11 */
#elif defined(__i386__)
__asm__(".text\n.globl " SYM(coro_swap) "\n" SYM(coro_swap) ":\n"
        "mov 4(%esp), %eax\n mov 8(%esp), %edx\n"
        "push %ebp\n push %ebx\n push %esi\n push %edi\n"
        "mov %esp, (%eax)\n mov %edx, %esp\n"
        "pop %edi\n pop %esi\n pop %ebx\n pop %ebp\n ret\n");
enum { SAVED = 4 };
#elif defined(__powerpc__) && !defined(__powerpc64__)
/* A 240-byte frame: back chain, r14-r31 at 8, LR at 80, CR at 84, f14-f31 at 88. */
__asm__(".text\n.globl " SYM(coro_swap) "\n" SYM(coro_swap) ":\n"
        "stwu 1, -240(1)\n mflr 0\n stw 0, 80(1)\n mfcr 0\n stw 0, 84(1)\n stmw 14, 8(1)\n"
        "stfd 14, 88(1)\n stfd 15, 96(1)\n stfd 16, 104(1)\n stfd 17, 112(1)\n stfd 18, 120(1)\n stfd 19, 128(1)\n stfd 20, 136(1)\n stfd 21, 144(1)\n stfd 22, 152(1)\n stfd 23, 160(1)\n stfd 24, 168(1)\n stfd 25, 176(1)\n stfd 26, 184(1)\n stfd 27, 192(1)\n stfd 28, 200(1)\n stfd 29, 208(1)\n stfd 30, 216(1)\n stfd 31, 224(1)\n "
        "stw 1, 0(3)\n mr 1, 4\n"
        "lwz 0, 80(1)\n mtlr 0\n lwz 0, 84(1)\n mtcrf 0xff, 0\n lmw 14, 8(1)\n"
        "lfd 14, 88(1)\n lfd 15, 96(1)\n lfd 16, 104(1)\n lfd 17, 112(1)\n lfd 18, 120(1)\n lfd 19, 128(1)\n lfd 20, 136(1)\n lfd 21, 144(1)\n lfd 22, 152(1)\n lfd 23, 160(1)\n lfd 24, 168(1)\n lfd 25, 176(1)\n lfd 26, 184(1)\n lfd 27, 192(1)\n lfd 28, 200(1)\n lfd 29, 208(1)\n lfd 30, 216(1)\n lfd 31, 224(1)\n "
        "addi 1, 1, 240\n blr\n");
enum { SAVED = 60 }; /* 240 bytes; LR (the resume address) is slot 20 */
#else
#error "coro_switch: no asm for this architecture"
#endif
typedef struct Coro { void *sp; void *caller_sp; void (*body)(struct Coro *); uintptr_t value; int done; unsigned char *stack; } Coro;
static Coro *current;
static void yield(Coro *c, uintptr_t v) { c->value = v; coro_swap(&c->sp, c->caller_sp); }
static void resume(Coro *c) { Coro *prev = current; current = c; coro_swap(&c->caller_sp, c->sp); current = prev; }
static void trampoline(void) { Coro *c = current; c->body(c); c->done = 1; coro_swap(&c->sp, c->caller_sp); abort(); }
static void start(Coro *c, void (*body)(Coro *), size_t size) {
    c->body = body; c->done = 0; c->stack = malloc(size);
    uintptr_t top = ((uintptr_t)c->stack + size) & ~(uintptr_t)15;
    void **sp;
#if defined(__aarch64__)
    sp = (void **)(top - 160);
    for (int i = 0; i < SAVED; ++i) sp[i] = 0;
    sp[11] = (void *)trampoline;
#elif defined(__powerpc__) && !defined(__powerpc64__)
    sp = (void **)(top - 240 - 16);
    for (int i = 0; i < SAVED; ++i) sp[i] = 0;
    sp[20] = (void *)trampoline;
#else
    /* [saved regs][return address = trampoline][fake return address], entered like a call */
    sp = (void **)(top - sizeof(void *) * 2) - SAVED;
    for (int i = 0; i < SAVED; ++i) sp[i] = 0;
    sp[SAVED] = (void *)trampoline;
    sp[SAVED + 1] = 0;
#endif
    c->sp = sp;
}
__attribute__((noinline)) static uintptr_t rec_yield(Coro *c, int depth, uintptr_t acc) {
    uintptr_t mine = acc * 3 + (uintptr_t)depth;
    if (depth == 0) { yield(c, mine); return mine + 1; }
    uintptr_t r = rec_yield(c, depth - 1, mine);
    yield(c, r ^ mine);
    return r + mine;
}
static void fib_body(Coro *c) { uintptr_t a = 0, b = 1; for (int i = 0; i < 20; ++i) { yield(c, a); uintptr_t t = a + b; a = b; b = t; } }
static void rec_body(Coro *c) { uintptr_t r = rec_yield(c, 6, 1); yield(c, r & 0xffffff); }
int main(void) {
    Coro f, g;
    start(&f, fib_body, 1 << 18);
    start(&g, rec_body, 1 << 18);
    unsigned long long sum = 0;
    int steps = 0;
    while (!f.done || !g.done) {
        if (!f.done) { resume(&f); if (!f.done) sum = sum * 31 + (f.value & 0xffff); }
        if (!g.done) { resume(&g); if (!g.done) { sum = sum * 7 + (g.value & 0xffffff); printf("g yields %lu\n", (unsigned long)(g.value & 0xffffff)); } }
        ++steps;
    }
    printf("steps=%d sum=%llu\n", steps, sum);
    free(f.stack); free(g.stack);
    return 0;
}
