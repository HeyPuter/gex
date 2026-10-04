/* PROVIDES ntdll.dll i386
 *
 * RtlQueryPerformanceCounter without a syscall. Wine's i386 ntdll makes
 * NtQueryPerformanceCounter for each call (no TSC bypass in this build),
 * and Noita's frame timer calls it about a thousand times a frame (70,000
 * syscalls a second, each a full trip through the NT layer). The counter
 * is the syscall's own (gex.nt_qpc: the process clock in 100 ns units, the
 * frequency RtlQueryPerformanceFrequency reports), so the two agree.
 *
 * BOOLEAN WINAPI RtlQueryPerformanceCounter(LARGE_INTEGER *counter):
 * stdcall, one argument; TRUE whatever the pointer, as Wine's. */

__attribute__((import_module("gex"), import_name("nt_qpc"))) long long gex_nt_qpc(void);

static GEX_PROVIDER(p_qpc) {
    (void)depth, (void)need;
    uint32_t out = gp_arg(ctx, 0);
    if (out >= 0x10000u) {
        uint64_t v = (uint64_t)gex_nt_qpc();
        GW(out) = (uint32_t)v;
        GW(out + 4) = (uint32_t)(v >> 32);
    }
    return gp_return_eax(ctx, ret, 4, 1);
}

GEX_PROVIDE(p_qpc, "RtlQueryPerformanceCounter")
