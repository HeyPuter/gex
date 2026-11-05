/* A signal sent to a running thread reports that thread's registers as they
 * were: whatever safepoint it lands at, the handler's ucontext holds the
 * live values. Wine's suspend (SIGUSR1, then the context in the signal frame
 * goes to the server and comes back on resume) depends on this, and a
 * register a safepoint left unspilled resumes as garbage.
 *
 * Each spinner holds a pattern in every general register it can and loops
 * in one shape: a plain back edge, an indirect jump, a call and return, a
 * system call, a loop whose body crosses into the next page. The main thread
 * sends SIGUSR1 to them round-robin. A handler that lands inside a loop
 * checks rip (inside that loop), rsp (the loop's, or one word below inside
 * the callee) and every register the loop does not change against the
 * pattern. */
#define _GNU_SOURCE
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

#define KINDS 5
#define N (2 * KINDS)

volatile int stop_flag;

/* spin_regs(base, &rsp, kind): the pattern is base + i in register i of
 * gregs' numbering (r8 0 .. r15 7, rdi 8, rsi 9, rbp 10, rbx 11, rdx 12,
 * rax 13, rcx 14). */
void spin_regs(unsigned long base, unsigned long *rsp_out, int kind);
extern const char k0_lo[], k0_hi[], k1_lo[], k1_hi[], k2_lo[], k2_hi[], k3_lo[], k3_hi[], k4_lo[], k4_hi[];

__asm__(
    ".text\n"
    ".globl spin_regs\n"
    ".type spin_regs,@function\n"
    "spin_regs:\n"
    "  push %rbp\n push %rbx\n push %r12\n push %r13\n push %r14\n push %r15\n"
    "  mov %rsp, (%rsi)\n"
    "  mov %edx, %eax\n"
    "  mov %rdi, %r11\n"
    "  lea 0(%r11), %r8\n lea 1(%r11), %r9\n lea 2(%r11), %r10\n"
    "  lea 4(%r11), %r12\n lea 5(%r11), %r13\n lea 6(%r11), %r14\n lea 7(%r11), %r15\n"
    "  lea 8(%r11), %rdi\n lea 9(%r11), %rsi\n lea 10(%r11), %rbp\n lea 11(%r11), %rbx\n"
    "  lea 12(%r11), %rdx\n lea 14(%r11), %rcx\n"
    "  cmp $1, %eax\n je 10f\n cmp $2, %eax\n je 20f\n cmp $3, %eax\n je 30f\n cmp $4, %eax\n je 40f\n"
    /* 0: a plain back edge */
    "  lea 13(%r11), %rax\n lea 3(%r11), %r11\n"
    ".globl k0_lo\n k0_lo:\n"
    "1: cmpl $0, stop_flag(%rip)\n je 1b\n"
    ".globl k0_hi\n k0_hi:\n"
    "  jmp 90f\n"
    /* 1: an indirect jump (r11 its target) */
    "10: lea 13(%r11), %rax\n lea 12f(%rip), %r11\n"
    ".globl k1_lo\n k1_lo:\n"
    "11: jmp *%r11\n"
    "12: cmpl $0, stop_flag(%rip)\n je 11b\n"
    ".globl k1_hi\n k1_hi:\n"
    "  jmp 90f\n"
    /* 2: a call and its return */
    "20: lea 13(%r11), %rax\n lea 3(%r11), %r11\n"
    ".globl k2_lo\n k2_lo:\n"
    "21: call 23f\n cmpl $0, stop_flag(%rip)\n je 21b\n jmp 24f\n"
    "23: ret\n"
    ".globl k2_hi\n k2_hi:\n"
    "24: jmp 90f\n"
    /* 3: a system call (getpid: rax, rcx, r11 change) */
    "30:\n"
    ".globl k3_lo\n k3_lo:\n"
    "31: mov $39, %eax\n syscall\n cmpl $0, stop_flag(%rip)\n je 31b\n"
    ".globl k3_hi\n k3_hi:\n"
    "  jmp 90f\n"
    /* 4: a loop whose body runs on into the next page */
    "40: lea 13(%r11), %rax\n lea 3(%r11), %r11\n jmp 41f\n"
    ".p2align 12\n"
    "  .fill 4080, 1, 0x90\n"
    ".globl k4_lo\n k4_lo:\n"
    "41: nop\n nop\n nop\n nop\n nop\n nop\n nop\n nop\n"
    "  nop\n nop\n nop\n nop\n nop\n nop\n nop\n nop\n nop\n nop\n nop\n nop\n"
    "  cmpl $0, stop_flag(%rip)\n je 41b\n"
    ".globl k4_hi\n k4_hi:\n"
    "90: pop %r15\n pop %r14\n pop %r13\n pop %r12\n pop %rbx\n pop %rbp\n"
    "  ret\n"
    ".size spin_regs, .-spin_regs\n");

static pthread_t th[N];
static unsigned long want_rsp[N];
static atomic_int up;
static atomic_long landed[N], outside, wrong;
static char why[512];
static atomic_int said;
static _Thread_local int me = -1;

static unsigned long base_of(int i) { return 0x5a5a000000000000ul | ((unsigned long)i << 32); }

static void say(const char *fmt, int i, int reg, unsigned long got, unsigned long want) {
    if (atomic_exchange(&said, 1)) return;
    snprintf(why, sizeof why, fmt, i, i % KINDS, reg, got, want);
}

static void on_usr1(int s, siginfo_t *si, void *uc_) {
    (void)s;
    (void)si;
    ucontext_t *uc = uc_;
    greg_t *g = uc->uc_mcontext.gregs;
    int i = me;
    if (i < 0) return;
    int kind = i % KINDS;
    const char *lo[KINDS] = {k0_lo, k1_lo, k2_lo, k3_lo, k4_lo};
    const char *hi[KINDS] = {k0_hi, k1_hi, k2_hi, k3_hi, k4_hi};
    unsigned long rip = (unsigned long)g[REG_RIP];
    if (rip < (unsigned long)lo[kind] || rip >= (unsigned long)hi[kind]) {
        atomic_fetch_add(&outside, 1);
        return;
    }
    atomic_fetch_add(&landed[i], 1);
    unsigned long base = base_of(i);
    for (int r = 0; r <= 14; r++) {
        if (r == 3 && (kind == 1 || kind == 3)) continue; /* r11: the jump target, or syscall's */
        if ((r == 13 || r == 14) && kind == 3) continue;  /* rax, rcx: syscall's */
        if ((unsigned long)g[r] != base + r) {
            atomic_fetch_add(&wrong, 1);
            say("thread %d (kind %d): gregs[%d] is %#lx, not %#lx\n", i, r, (unsigned long)g[r], base + r);
        }
    }
    unsigned long rsp = (unsigned long)g[REG_RSP];
    if (rsp != want_rsp[i] && !(kind == 2 && rsp == want_rsp[i] - 8)) {
        atomic_fetch_add(&wrong, 1);
        say("thread %d (kind %d): rsp (%d) is %#lx, not %#lx\n", i, REG_RSP, rsp, want_rsp[i]);
    }
}

static void *spinner(void *arg) {
    me = (int)(long)arg;
    atomic_fetch_add(&up, 1);
    spin_regs(base_of(me), &want_rsp[me], me % KINDS);
    return 0;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_usr1;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    sigaction(SIGUSR1, &sa, 0);
    for (int i = 0; i < N; i++) pthread_create(&th[i], 0, spinner, (void *)(long)i);
    while (atomic_load(&up) < N) sched_yield();
    struct timespec t0, t, gap = {0, 200000};
    clock_gettime(CLOCK_MONOTONIC, &t0);
    long need = 20;
    for (long sent = 0;; sent++) {
        pthread_kill(th[sent % N], SIGUSR1);
        if (sent % N == N - 1) nanosleep(&gap, 0);
        clock_gettime(CLOCK_MONOTONIC, &t);
        double s = (t.tv_sec - t0.tv_sec) + (t.tv_nsec - t0.tv_nsec) / 1e9;
        int all = 1;
        for (int i = 0; i < N; i++) all &= atomic_load(&landed[i]) >= need;
        if ((all && s > 0.5) || s > 20) break;
    }
    stop_flag = 1;
    for (int i = 0; i < N; i++) pthread_join(th[i], 0);
    int ok = atomic_load(&wrong) == 0;
    for (int i = 0; i < N; i++) {
        if (atomic_load(&landed[i]) < need) {
            ok = 0;
            printf("thread %d (kind %d): only %ld signals landed in its loop\n", i, i % KINDS, atomic_load(&landed[i]));
        }
    }
    if (atomic_load(&wrong)) printf("%ld wrong registers; first: %s", atomic_load(&wrong), why);
    printf(ok ? "sig-regs-threads: ok\n" : "sig-regs-threads: FAILED\n");
    return ok ? 0 : 1;
}
