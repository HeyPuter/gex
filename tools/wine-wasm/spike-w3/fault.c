/* W3.1: faults in x86 code a native wasm32 program runs reach its fault
 * hook (gex.fault_hook), which points the guest at a recovery stub; the
 * call-out the code ran under (gex.call_out) then returns normally, the
 * native frames under it intact. One case per fault kind. */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include "wine/gex/gex_abi.h"
#include "wine/gex/provider.h"
unsigned long __gex_get_ctx(void);
__attribute__((import_module("gex"), import_name("fault_hook"))) void gex_fault_hook(int idx);

static uint32_t recover;    /* x86: mov eax, 7; ret */
static unsigned last_code, last_pc;

static void hook(int ctx, unsigned code, unsigned addr, unsigned kind)
{
    (void)addr;
    last_code = code;
    last_pc = (unsigned)GEX_C64(GEX_CTX_PC);
    GEX_C64(GEX_CTX_PC) = recover;
}

static int run(uint32_t target)
{
    int ctx = (int)__gex_get_ctx();
    uint32_t sp0 = gp_reg(ctx, R_ESP), sp = (sp0 - 64) & ~15u, ra = target ^ 1u;
    GW(sp) = ra;
    gp_set(ctx, R_ESP, sp);
    gp_set(ctx, R_EAX, 0);
    int ok = gex_call_out(ctx, target, ra, sp + 4);
    unsigned eax = gp_reg(ctx, R_EAX);
    gp_set(ctx, R_ESP, sp0);
    return ok ? (int)eax : -1;
}

int main(void)
{
    static const unsigned char rec[] = { 0xb8, 7, 0, 0, 0, 0xc3 };
    static const unsigned char mem[] = { 0xb8, 0x34, 0x12, 0, 0, 0xa1, 0, 0xf0, 0xff, 0xff, 0xc3 }; /* mov eax,[0xfffff000] */
    static const unsigned char brk[] = { 0x90, 0xcc, 0xc3 };                                     /* int3 */
    static const unsigned char ill[] = { 0x90, 0x0f, 0x0b, 0xc3 };                               /* ud2 */
    static const unsigned char div[] = { 0x31, 0xc9, 0x31, 0xd2, 0xb8, 1, 0, 0, 0, 0xf7, 0xf1, 0xc3 }; /* div ecx (0) */
    unsigned char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memcpy(p, rec, sizeof rec);
    memcpy(p + 0x100, mem, sizeof mem);
    memcpy(p + 0x200, brk, sizeof brk);
    memcpy(p + 0x300, ill, sizeof ill);
    memcpy(p + 0x400, div, sizeof div);
    mprotect(p, 4096, PROT_READ | PROT_EXEC);
    recover = (uint32_t)(uintptr_t)p;
    gex_fault_hook((int)(uintptr_t)hook);
    static const char *names[] = { "memory", "int3", "ud2", "divide" };
    for (int i = 0; i < 4; i++) {
        last_code = 0;
        int r = run(recover + 0x100 * (i + 1));
        printf("%s: returned %d, fault %#x at +%#x\n", names[i], r, last_code, last_pc ? last_pc - recover : 0);
    }
    return 0;
}
