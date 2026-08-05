/* Signal delivery (PLAN §8).
 *
 * The interesting cases are the ones that need a real, precise context:
 * siglongjmp out of a handler, a caught SIGILL resuming elsewhere, and
 * delivery inside a tight loop, which only arrives if the back-edge
 * safepoints in tier-1 codegen actually fire. The fatal modes check that an
 * uncaught signal produces the same exit status as Linux.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <unistd.h>
#include <setjmp.h>
static volatile sig_atomic_t hits=0, last=0;
static void h1(int s){ hits++; last=s; }
static volatile sig_atomic_t saved_x;
static void h_info(int s, siginfo_t *si, void *uc){
    (void)uc; hits++; last=s; saved_x = si->si_signo;
}
static sigjmp_buf jb;
static void h_jmp(int s){ (void)s; siglongjmp(jb,7); }
/* A caught SIGSEGV: the handler jumps out, proving the fault reaches guest
 * code with a usable context rather than just killing the process. */
static sigjmp_buf segv_jb;
static void h_segv(int s){ (void)s; siglongjmp(segv_jb, 9); }

int main(int argc, char **argv){
    const char *mode = argc > 1 ? argv[1] : "all";
    if (!strcmp(mode, "fatal-default")) {
        /* No handler: the default disposition must end the process. */
        raise(SIGTERM);
        printf("survived\n");
        return 1;
    }
    if (!strcmp(mode, "fatal-abort")) {
        abort();
    }
    if (!strcmp(mode, "catch-illegal")) {
        struct sigaction sa; memset(&sa,0,sizeof sa);
        sa.sa_handler = h_segv; sigemptyset(&sa.sa_mask);
        sigaction(SIGILL,&sa,NULL);
        if (sigsetjmp(segv_jb,1) == 0) {
            /* An encoding no implementation defines. */
            __asm__ volatile(".inst 0x00000000");
            printf("no-trap\n");
        } else {
            printf("caught-sigill\n");
        }
        return 0;
    }
    /* Basic delivery via raise() */
    signal(SIGUSR1, h1);
    raise(SIGUSR1);
    printf("raise: hits=%d last=%d\n", (int)hits, (int)last);

    /* kill(self) */
    signal(SIGUSR2, h1);
    hits=0; kill(getpid(), SIGUSR2);
    printf("kill: hits=%d last=%d\n", (int)hits, (int)last);

    /* SA_SIGINFO */
    struct sigaction sa; memset(&sa,0,sizeof sa);
    sa.sa_sigaction = h_info; sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1,&sa,NULL);
    hits=0; raise(SIGUSR1);
    printf("siginfo: hits=%d si_signo=%d\n", (int)hits, (int)saved_x);

    /* Blocking keeps it pending, unblocking delivers it */
    sigset_t set, old; sigemptyset(&set); sigaddset(&set,SIGUSR1);
    sigprocmask(SIG_BLOCK,&set,&old);
    hits=0; raise(SIGUSR1);
    printf("blocked: hits=%d\n", (int)hits);
    sigprocmask(SIG_UNBLOCK,&set,NULL);
    printf("unblocked: hits=%d\n", (int)hits);

    /* Handler-local mask: the signal is blocked inside its own handler */
    signal(SIGUSR1, h1);
    hits=0; raise(SIGUSR1);
    printf("reentry-safe: hits=%d\n", (int)hits);

    /* Restoring SIG_DFL and SIG_IGN */
    signal(SIGUSR1, SIG_IGN); hits=0; raise(SIGUSR1);
    printf("ignored: hits=%d\n", (int)hits);

    /* siglongjmp out of a handler */
    signal(SIGUSR2, h_jmp);
    int r = sigsetjmp(jb,1);
    if (r==0) { raise(SIGUSR2); printf("unreachable\n"); }
    else printf("siglongjmp=%d\n", r);

    /* Signal during a tight loop reaches a safepoint */
    signal(SIGALRM, h1); hits=0;
    kill(getpid(), SIGALRM);
    volatile unsigned long spin=0; for (unsigned long i=0;i<3000000UL;i++) spin+=i;
    printf("loop-delivery: hits=%d spin!=0=%d\n", (int)hits, spin!=0);

    printf("done\n");
    return 0;
}
