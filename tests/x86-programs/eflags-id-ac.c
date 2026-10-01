/* The EFLAGS bits user code can toggle: ID (bit 21), whose toggling is how a
 * program learns cpuid exists (LuaJIT's lj_vm_cpuid does exactly this), and
 * AC (bit 18). Then cpuid's leaf 0 and 1 for what such a probe goes on to
 * read: a vendor string and the CMOV/SSE2 bits a JIT needs. The values
 * themselves are the host's natively and gex's under gex, so only what
 * must agree is printed. */
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <ucontext.h>
#include <stdio.h>
#include <string.h>

static uintptr_t flags(void) {
    uintptr_t f;
    __asm__ volatile("pushf\n pop %0" : "=r"(f));
    return f;
}

static void set_flags(uintptr_t f) {
    __asm__ volatile("push %0\n popf" : : "r"(f) : "cc");
}

static int toggles(uintptr_t bit) {
    uintptr_t before = flags();
    set_flags(before ^ bit);
    uintptr_t after = flags();
    set_flags(before);
    uintptr_t restored = flags();
    return ((after ^ before) & bit) != 0 && ((restored ^ before) & bit) == 0;
}

/* A signal frame carries ID too: the handler sees it in the saved EFLAGS,
 * and sigreturn puts it back. */
static volatile int handler_saw_id;
static void on_usr1(int sig, siginfo_t *si, void *uc) {
    (void)sig; (void)si;
    handler_saw_id = (((ucontext_t *)uc)->uc_mcontext.gregs[REG_EFL] >> 21) & 1;
}

static void cpuid(uint32_t leaf, uint32_t r[4]) {
    __asm__ volatile("cpuid" : "=a"(r[0]), "=b"(r[1]), "=c"(r[2]), "=d"(r[3]) : "a"(leaf), "c"(0));
}

int main(void) {
    printf("ID toggles: %d\n", toggles(1u << 21));
    printf("AC toggles: %d\n", toggles(1u << 18));
    /* LuaJIT's probe, as lj_vm_cpuid writes it for i386. */
#ifdef __i386__
    uint32_t f, g;
    __asm__ volatile("pushf\n pop %%eax\n mov %%eax, %%ecx\n xor $0x200000, %%eax\n push %%eax\n popf\n pushf\n pop %%eax\n"
                     "push %%ecx\n popf"
                     : "=a"(f), "=c"(g) : : "cc");
    printf("has cpuid: %d\n", (f ^ g) != 0);
#else
    printf("has cpuid: %d\n", toggles(1u << 21));
#endif
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_usr1;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGUSR1, &sa, 0);
    uintptr_t before = flags();
    set_flags(before | (1u << 21));
    raise(SIGUSR1);
    uintptr_t after = flags();
    set_flags(before);
    printf("signal frame keeps ID: %d, sigreturn restores ID: %d\n", handler_saw_id, (int)((after >> 21) & 1));
    uint32_t r[4];
    char vendor[13];
    cpuid(0, r);
    memcpy(vendor, &r[1], 4);
    memcpy(vendor + 4, &r[3], 4);
    memcpy(vendor + 8, &r[2], 4);
    vendor[12] = 0;
    int known = !strcmp(vendor, "GenuineIntel") || !strcmp(vendor, "AuthenticAMD");
    printf("max leaf >= 1: %d, known vendor: %d\n", r[0] >= 1, known);
    cpuid(1, r);
    printf("cmov %d sse %d sse2 %d\n", (r[3] >> 15) & 1, (r[3] >> 25) & 1, (r[3] >> 26) & 1);
    return 0;
}
