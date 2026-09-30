/* x86 #DE: div/idiv by zero and quotient overflow at every width raise
 * SIGFPE (FPE_INTDIV) at the dividing instruction, before any register
 * changes; the in-range cases compute normally. Prints one line per case,
 * to be compared with native execution. */
#define _GNU_SOURCE
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <ucontext.h>

#ifdef __x86_64__
#define PC REG_RIP
#define RA REG_RAX
#define RD REG_RDX
typedef uint64_t reg;
#define LEA_AT "lea 1f(%%rip), %3\nmov %3, insn_at(%%rip)\n"
#else
#define PC REG_EIP
#define RA REG_EAX
#define RD REG_EDX
typedef uint32_t reg;
#define LEA_AT "lea 1f, %3\nmov %3, insn_at\n"
#endif

static sigjmp_buf jb;
static volatile int got_code;
static volatile reg fault_pc, fault_addr, fault_a, fault_d;
reg insn_at;

static void handler(int sig, siginfo_t *si, void *ucv) {
    ucontext_t *uc = ucv;
    got_code = si->si_code;
    fault_addr = (reg)(uintptr_t)si->si_addr;
    fault_pc = uc->uc_mcontext.gregs[PC];
    fault_a = uc->uc_mcontext.gregs[RA];
    fault_d = uc->uc_mcontext.gregs[RD];
    siglongjmp(jb, 1);
}

/* One division: `insn` with a, d preloaded, divisor in a register of the
 * width; the address of the instruction comes back in `at`. */
#define CASE(name, insn, dreg, A, D, V) do { \
    reg a = (reg)(A), d = (reg)(D), v = (reg)(V), at; \
    got_code = -1; \
    if (!sigsetjmp(jb, 1)) { \
        __asm__ volatile(LEA_AT "1: " insn " %" dreg "4" \
            : "+a"(a), "+d"(d), "=m"(v), "=&r"(at) : "c"(v) : "cc", "memory"); \
        printf("%-8s ok  a=%llx d=%llx\n", name, (unsigned long long)a, (unsigned long long)d); \
    } else { \
        printf("%-8s SIGFPE code=%d at_insn=%d addr_ok=%d a=%llx d=%llx\n", name, got_code, \
               fault_pc == insn_at, fault_addr == fault_pc, (unsigned long long)fault_a, (unsigned long long)fault_d); \
    } \
} while (0)

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigaction(SIGFPE, &sa, 0);
    setvbuf(stdout, 0, _IONBF, 0);
    /* 8-bit: ax / r8 */
    CASE("div8/0", "divb", "b", 0x1234, 0, 0);
    CASE("div8/ov", "divb", "b", 0x1234, 0, 0x10);
    CASE("div8", "divb", "b", 0x1234, 0, 0x13);
    CASE("idiv8/ov", "idivb", "b", 0x8000, 0, 0xff);
    CASE("idiv8/m", "idivb", "b", 0xff00, 0, 2);
    CASE("idiv8/p", "idivb", "b", 0x100, 0, 2);
    /* 16-bit: dx:ax / r16 */
    CASE("div16/0", "divw", "w", 0x1234, 0x5, 0);
    CASE("div16/ov", "divw", "w", 0x1234, 0x5, 0x5);
    CASE("div16", "divw", "w", 0x1234, 0x5, 0x6);
    CASE("idiv16/o", "idivw", "w", 0x0000, 0x8000, 0xffff);
    CASE("idiv16/n", "idivw", "w", 0x8000, 0xffff, 1);
    CASE("idiv16/p", "idivw", "w", 0x8000, 0x0000, 1);
    /* 32-bit: edx:eax / r32 */
    CASE("div32/0", "divl", "k", 0x12345678, 0x9, 0);
    CASE("div32/ov", "divl", "k", 0x12345678, 0x9, 0x9);
    CASE("div32", "divl", "k", 0x12345678, 0x9, 0xa);
    CASE("idiv32/o", "idivl", "k", 0x80000000, 0xffffffff, 0xffffffff);
    CASE("idiv32/n", "idivl", "k", 0x80000000, 0xffffffff, 1);
    CASE("idiv32/p", "idivl", "k", 0x80000000, 0, 1);
    CASE("idiv32", "idivl", "k", 0xfffffff9, 0xffffffff, 2);
#ifdef __x86_64__
    /* 64-bit: rdx:rax / r64 */
    CASE("div64/0", "divq", "q", 0x123456789abcdef0ull, 0x7, 0);
    CASE("div64/ov", "divq", "q", 0x123456789abcdef0ull, 0x7, 0x7);
    CASE("div64", "divq", "q", 0x123456789abcdef0ull, 0x7, 0x8);
    CASE("idiv64/o", "idivq", "q", 0x8000000000000000ull, ~0ull, ~0ull);
    CASE("idiv64/n", "idivq", "q", 0x8000000000000000ull, ~0ull, 1);
    CASE("idiv64/p", "idivq", "q", 0x8000000000000000ull, 0, 1);
    CASE("idiv64/w", "idivq", "q", 0, 1, 2);
    CASE("idiv64/x", "idivq", "q", 0, 0xffffffffffffffffull, 0x8000000000000000ull);
    CASE("idiv64", "idivq", "q", 0xfffffffffffffff9ull, ~0ull, 2);
#endif
    return 0;
}
