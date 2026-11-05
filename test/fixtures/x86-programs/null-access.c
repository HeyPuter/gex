/* Reads and writes through null pointers fault, as on Linux, whose lowest
 * 64 KiB are never mapped (vm.mmap_min_addr): gex once read them as memory,
 * so a NULL table pointer was followed and the program went on with zeros
 * (Paint's lpk). Each probe takes SIGSEGV in a handler that jumps
 * back out; the last read has no handler and ends the process with SIGSEGV.
 *
 * Every probe is its own function so the faulting access is the first thing
 * after the sigsetjmp in a fresh block: the handler only needs a stack to run
 * on, not the faulting instruction's exact registers. */
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static sigjmp_buf back;
static volatile int faults;

static void on_segv(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)si;
    (void)uc;
    faults++;
    siglongjmp(back, 1);
}

static volatile uintptr_t where;

__attribute__((noinline)) static int load32(void) { return *(volatile int *)where; }
__attribute__((noinline)) static int load8(void) { return *(volatile unsigned char *)where; }
__attribute__((noinline)) static void store32(int v) { *(volatile int *)where = v; }
__attribute__((noinline)) static long long load64(void) { return *(volatile long long *)where; }
__attribute__((noinline)) static int field(void) {
    struct s { int a[16]; } *volatile p = (struct s *)where;
    return p->a[9];
}

static void probe(const char *what, uintptr_t at, int (*f)(void)) {
    where = at;
    if (sigsetjmp(back, 1) == 0) {
        int v = f();
        printf("%s at %#lx read %d (no fault)\n", what, (unsigned long)at, v);
    } else {
        printf("%s at %#lx faulted\n", what, (unsigned long)at);
    }
    fflush(stdout);
}

static int do_load64(void) { return (int)load64(); }
static int do_store(void) {
    store32(42);
    return 0;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, 0);

    probe("load32", 0, load32);
    probe("load8", 1, load8);
    probe("load64", 0x20, do_load64);
    probe("store32", 0, do_store);
    probe("field", 0, field);
    probe("load32", 0x1000, load32);
    probe("load32", 0xfffc, load32);
    printf("faults %d\n", faults);
    fflush(stdout);

    /* No handler: the process dies of the signal. */
    signal(SIGSEGV, SIG_DFL);
    where = 0;
    printf("last %d\n", load32());
    return 0;
}
