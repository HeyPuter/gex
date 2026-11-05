/* A process that faults dies of the signal Linux gives the fault, not of
 * SIGKILL. Each case runs in a forked child; the parent prints what its
 * wait status says.
 *
 *   faultsig            all cases
 *   faultsig CASE       one case, in this process (no fork)
 *
 * Cases and Linux's answer:
 *   nullcall   call through a null function pointer     SIGSEGV
 *   wildcall   call through a garbage function pointer  SIGSEGV
 *   vcall      a virtual-style call on a null object    SIGSEGV
 *   divzero    integer division by zero                 SIGFPE
 *   trap       __builtin_trap                           SIGILL
 *   recurse    unbounded recursion                      SIGSEGV
 *   handler    nullcall with a SIGSEGV handler that _exits(42)  exit 42
 *              (gex: SIGSEGV. A wasm32 program's handler runs from a frame
 *              its own code finds at a syscall's return, and the frames
 *              that trapped are gone, so it dies of the signal unhandled;
 *              either answer passes.) */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

typedef int (*fn_t)(int);
struct obj { const fn_t *vt; };
static volatile fn_t null_fn;
static volatile unsigned long wild = 0x7ffff123;
static struct obj *volatile null_obj;
static volatile int zero;

static int deep(int n) {
    volatile char pad[256];
    pad[0] = (char)n;
    return deep(n + 1) + pad[0];
}

static void on_segv(int s) { (void)s; _exit(42); }

static int run(const char *c) {
    if (!strcmp(c, "nullcall")) return null_fn(1);
    if (!strcmp(c, "wildcall")) return ((fn_t)wild)(1);
    if (!strcmp(c, "vcall")) return null_obj->vt[2](1);
    if (!strcmp(c, "divzero")) return 7 / zero;
    if (!strcmp(c, "trap")) __builtin_trap();
    if (!strcmp(c, "recurse")) return deep(0);
    if (!strcmp(c, "handler")) {
        signal(SIGSEGV, on_segv);
        return null_fn(1);
    }
    fprintf(stderr, "unknown case %s\n", c);
    return 99;
}

int main(int argc, char **argv) {
    if (argc > 1) return run(argv[1]);
    static const struct { const char *c; int sig, code; } want[] = {
        {"nullcall", SIGSEGV, 0}, {"wildcall", SIGSEGV, 0}, {"vcall", SIGSEGV, 0},
        {"divzero", SIGFPE, 0},   {"trap", SIGILL, 0},      {"recurse", SIGSEGV, 0},
        {"handler", 0, 42},
    };
    int bad = 0;
    for (unsigned i = 0; i < sizeof want / sizeof want[0]; i++) {
        fflush(stdout);
        pid_t p = fork();
        if (p == 0) _exit(run(want[i].c) & 0x7f);
        int st = 0;
        waitpid(p, &st, 0);
        int ok = want[i].sig ? (WIFSIGNALED(st) && WTERMSIG(st) == want[i].sig)
                             : (WIFEXITED(st) && WEXITSTATUS(st) == want[i].code) ||
                                   (!strcmp(want[i].c, "handler") && WIFSIGNALED(st) && WTERMSIG(st) == SIGSEGV);
        /* Only the verdict on a pass: run.sh compares this with the host's
         * output, and libc names signals its own way. */
        if (ok)
            printf("%-9s ok\n", want[i].c);
        else if (WIFSIGNALED(st))
            printf("%-9s signal %d  WRONG\n", want[i].c, WTERMSIG(st));
        else
            printf("%-9s exit %d  WRONG\n", want[i].c, WEXITSTATUS(st));
        bad += !ok;
    }
    printf("faultsig: %s\n", bad ? "FAIL" : "ok");
    return bad != 0;
}
