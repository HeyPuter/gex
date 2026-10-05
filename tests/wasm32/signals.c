/* gex-kernel's wasm32 personality: signals delivered at a syscall's return
 * and run by musl's syscall stub. A plain handler, an SA_SIGINFO handler
 * with its siginfo, the mask during and after a handler, a signal from
 * another process (kill), and a syscall result surviving the handler. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
#include <spawn.h>

static volatile int plain, info_sig, info_code, info_pid, masked_inside;

static void on_plain(int s) { plain = s; }

static void on_info(int s, siginfo_t *si, void *uc)
{
    sigset_t cur;
    info_sig = si->si_signo;
    info_code = si->si_code;
    info_pid = si->si_pid;
    sigprocmask(SIG_SETMASK, 0, &cur);
    masked_inside = sigismember(&cur, s);
    (void)uc;
}

int main(void)
{
    signal(SIGUSR1, on_plain);
    raise(SIGUSR1);
    printf("plain handler: %d\n", plain == SIGUSR1);

    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_info;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR2, &sa, 0);
    kill(getpid(), SIGUSR2);
    printf("siginfo handler: signo %d, code SI_USER %d, pid ours %d, masked inside %d\n",
           info_sig == SIGUSR2, info_code == SI_USER, info_pid == getpid(), masked_inside);
    sigset_t cur;
    sigprocmask(SIG_SETMASK, 0, &cur);
    printf("unmasked after: %d\n", !sigismember(&cur, SIGUSR2));

    /* the syscall that the signal interrupted still returns its result */
    plain = 0;
    pid_t me = getpid();
    long r = kill(me, SIGUSR1);
    printf("kill result %ld, then getpid matches %d, handler ran %d\n", r, getpid() == me, plain == SIGUSR1);

    /* blocked, then delivered on unblock */
    sigset_t s1;
    sigemptyset(&s1);
    sigaddset(&s1, SIGUSR1);
    sigprocmask(SIG_BLOCK, &s1, 0);
    plain = 0;
    raise(SIGUSR1);
    int before = plain;
    sigprocmask(SIG_UNBLOCK, &s1, 0);
    printf("blocked: not before %d, delivered on unblock %d\n", before == 0, plain == SIGUSR1);
    return 0;
}
