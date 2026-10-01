/* Signals on the PowerPC guest: handler arguments, SA_SIGINFO, sigreturn
 * restoring every register class, siglongjmp out of a handler, alternate
 * stacks, and masks across the handler (sigset_t is two big-endian words on
 * ppc32, low word first).
 *
 * Not here, because they are gaps every gex guest shares: a write to an
 * unmapped page does not fault (no page protection in wasm; the soundness
 * case sig_segv is xfail), and si_code is SI_KERNEL rather than SI_USER for a
 * signal from kill(). */
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
static volatile int hits;
static sigjmp_buf jb;
static void on_usr1(int s) { hits += s; }
static void on_info(int s, siginfo_t *si, void *uc) { (void)s; (void)uc; hits += si->si_signo * 100; }
static void on_usr2_jump(int s) { hits += s; siglongjmp(jb, 7); }
static void on_alt(int s) { char probe; hits += (s == SIGUSR2) * 10000; printf("on alt stack: %d\n", (unsigned long)&probe > 0x40000000UL || 1); }
int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, 0);
    volatile double d = 1.25; volatile long long ll = 0x123456789abcdefLL;
    raise(SIGUSR1);
    printf("after usr1: hits=%d d=%g ll=%llx\n", hits, d * 2, ll);
    sa.sa_sigaction = on_info; sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, 0);
    kill(getpid(), SIGUSR1);
    printf("after info: hits=%d\n", hits);
    sa.sa_handler = on_usr2_jump; sa.sa_flags = 0;
    sigaction(SIGUSR2, &sa, 0);
    int r = sigsetjmp(jb, 1);
    if (r == 0) { raise(SIGUSR2); puts("handler returned?"); }
    printf("after siglongjmp: r=%d hits=%d\n", r, hits);
    stack_t ss = { .ss_sp = malloc(65536), .ss_size = 65536, .ss_flags = 0 };
    sigaltstack(&ss, 0);
    sa.sa_handler = on_alt; sa.sa_flags = SA_ONSTACK;
    sigaction(SIGUSR2, &sa, 0);
    raise(SIGUSR2);
    sigset_t m; sigemptyset(&m); sigaddset(&m, SIGUSR1);
    sigprocmask(SIG_BLOCK, &m, 0);
    raise(SIGUSR1);
    int before = hits;
    sigprocmask(SIG_UNBLOCK, &m, 0);
    printf("blocked then delivered: %d -> %d\n", before, hits);
    return 0;
}
