#define _GNU_SOURCE
/* W3.0 spike: a fault in x86 code reaches the wasm32 program's SIGSEGV
 * handler, with the x86 state in its ucontext (ntdll.so's segv_handler
 * builds the EXCEPTION_RECORD from it). x86: mov eax,0x1234; mov eax,[0]; ret */
#include <signal.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <setjmp.h>
#include <ucontext.h>
#include <sys/mman.h>
#include "wine/gex/gex_abi.h"
#include "wine/gex/provider.h"
unsigned long __gex_get_ctx(void);
static sigjmp_buf env;

static void handler(int sig, siginfo_t *si, void *uc_)
{
    ucontext_t *uc = uc_;
    printf("signal %d at addr %p; ucontext eip=%#lx eax=%#lx\n", sig, si->si_addr,
           (unsigned long)uc->uc_mcontext.gregs[REG_EIP], (unsigned long)uc->uc_mcontext.gregs[REG_EAX]);
    fflush(stdout);
    siglongjmp(env, 1);
}

int main(void)
{
    static const unsigned char code[] = { 0xb8, 0x34, 0x12, 0, 0, 0xa1, 0, 0xf0, 0xff, 0xff, 0xc3 };
    unsigned char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memcpy(p, code, sizeof code);
    mprotect(p, 4096, PROT_READ | PROT_EXEC);
    struct sigaction sa = {0};
    sa.sa_sigaction = handler;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, 0);
    printf("code at %p\n", p);
    if (!sigsetjmp(env, 1)) {
        int ctx = (int)__gex_get_ctx();
        uint32_t sp0 = gp_reg(ctx, R_ESP), sp = (sp0 - 64) & ~15u, target = (uint32_t)(uintptr_t)p;
        GW(sp) = target ^ 1u;
        gp_set(ctx, R_ESP, sp);
        GEX_C64(GEX_CTX_LAST_TARGET) = target;
        GEX_FTAB(gex_ftab_idx(target))(ctx, 1, 0xffffffffu, target ^ 1u);
        printf("returned (no fault?)\n");
    } else {
        printf("back in main after the handler\n");
    }
    return 0;
}
