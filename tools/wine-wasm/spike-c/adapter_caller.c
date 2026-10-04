/* The x86 side of the adapter benchmark (adapter_bench.mjs): a unit's call
 * sequence into a winebuild-generated entry adapter, through the shared
 * function table, as gex-cgen emits it for a cross-image stdcall call. */
#include <stdint.h>

#define CTX 0x1000u
#define X(n) (*(volatile uint64_t *)(uintptr_t)(CTX + 8 * (n)))
#define G32(a) (*(volatile uint32_t *)(uintptr_t)(a))

typedef int (*fbody)(int ctx, int depth, int need, int64_t ret);

/* n stdcall calls of f(i, s) at table index idx; returns the last result */
int bench_call(int n, int idx)
{
    int s = 0;
    fbody f = (fbody)(uintptr_t)idx;
    _Pragma("clang loop unroll(disable)") for (int i = 0; i < n; i++)
    {
        uint32_t sp = (uint32_t)X(4) - 12, ra = 0x401234;
        G32(sp) = ra;
        G32(sp + 4) = i;
        G32(sp + 8) = s;
        X(4) = sp;
        X(44) = 0x7b801000;  /* LAST_TARGET */
        if (!f(CTX, 1, 0, ra)) __builtin_trap();
        s = (int)(uint32_t)X(0);  /* stdcall: the callee popped */
    }
    return s;
}
