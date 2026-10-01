/* Privileged instructions in user mode raise #GP, which Linux delivers as
 * SIGSEGV at the instruction: hlt (musl's a_crash), cli, sti, in, out. A
 * SIGSEGV handler reports each and jumps past it. */
#define _GNU_SOURCE
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>

static sigjmp_buf jb;
static volatile int got;

static void handler(int sig, siginfo_t *si, void *uc) {
    (void)si;
    (void)uc;
    got = sig;
    siglongjmp(jb, 1);
}

#define TRY(name, insn) do { \
    got = 0; \
    if (!sigsetjmp(jb, 1)) __asm__ volatile(insn ::: "memory"); \
    printf("%-4s signal %d\n", name, got); \
} while (0)

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigaction(SIGSEGV, &sa, 0);
    setvbuf(stdout, 0, _IONBF, 0);
    TRY("hlt", "hlt");
    TRY("cli", "cli");
    TRY("sti", "sti");
    TRY("in", "inb $0x60, %%al");
    TRY("out", "outb %%al, $0x80");
    TRY("inx", "inb %%dx, %%al");
    return 0;
}
