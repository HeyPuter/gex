/* W3.0 spike: a native wasm32 program (the shape of Wine's loader) maps x86
 * code and calls it through gex's function table, as ntdll.so will call
 * the program's code. x86: mov eax,[esp+4]; add eax,[esp+8]; ret */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include "wine/gex/gex_abi.h"
#include "wine/gex/provider.h"
unsigned long __gex_get_ctx(void);

static uint64_t guest_call(uint32_t target, const uint32_t *words, uint32_t n)
{
    int ctx = (int)__gex_get_ctx();
    uint32_t sp0 = gp_reg(ctx, R_ESP);
    uint32_t sp = (sp0 - 4 * n - 4 - 64) & ~15u;
    uint32_t ra = target ^ 1u;
    GW(sp) = ra;
    for (uint32_t i = 0; i < n; i++) GW(sp + 4 + 4 * i) = words[i];
    gp_set(ctx, R_ESP, sp);
    GEX_C64(GEX_CTX_LAST_TARGET) = target;
    int ok = GEX_FTAB(gex_ftab_idx(target))(ctx, 1, 0xffffffffu, ra);
    gp_set(ctx, R_ESP, sp0);
    if (!ok) printf("guest left another way, pc %#llx\n", (unsigned long long)GEX_C64(GEX_CTX_PC));
    return gp_reg(ctx, R_EAX);
}

int main(void)
{
    static const unsigned char code[] = { 0x8b, 0x44, 0x24, 0x04, 0x03, 0x44, 0x24, 0x08, 0xc3 };
    unsigned char *p = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    memcpy(p, code, sizeof code);
    mprotect(p, 4096, PROT_READ | PROT_EXEC);
    uint32_t args[2] = { 40, 2 };
    printf("x86 add(40, 2) = %llu\n", (unsigned long long)guest_call((uint32_t)(uintptr_t)p, args, 2));
    return 0;
}
