/* hooktest.exe (x64): hooks of Wine builtin exports the way hooking libraries write them, on a
 * split DLL's export (user32 GetSystemMetrics) and a core one's (kernelbase GetCurrentProcessId):
 *   abs14  a 14-byte jmp [rip+0] + address at the export; the trampoline is the export's first 14
 *          bytes copied, then a jmp back (a raw hook with no instruction decoder)
 *   rel5   a 5-byte jmp rel32 to a relay near the export (MinHook, Detours); the trampoline is the
 *          export's first instruction relocated (a RIP-relative operand re-aimed), then a jmp back
 * Each: the hook runs and its trampoline reaches the original, for a call through GetProcAddress's
 * pointer and through the import table, and after the unhook the export runs unhooked again.
 * Prints "hooktest CASE ok" per case, "hooktest: N failures" at the end.
 * gex-nt-arch: x64
 */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
static void check(const char *what, int ok) { if (!ok) { printf("hooktest FAIL %s\n", what); failures++; } }

static int (WINAPI *orig_gsm)(int);
static DWORD (WINAPI *orig_gcpi)(void);
static int hits;
static int WINAPI hook_gsm(int i) { hits++; return orig_gsm(i); }
static DWORD WINAPI hook_gcpi(void) { hits++; return orig_gcpi(); }

/* a page of executable memory within 1 GiB of `near` (a rel32 reaches it) */
static BYTE *alloc_near(BYTE *at)
{
    SYSTEM_INFO si; GetSystemInfo(&si);
    ULONG_PTR g = si.dwAllocationGranularity, base = (ULONG_PTR)at & ~(g - 1), d;
    for (d = g; d < 0x40000000; d += g)
    {
        void *p;
        if ((p = VirtualAlloc((void *)(base + d), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE))) return p;
        if (base > d && (p = VirtualAlloc((void *)(base - d), 4096, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE))) return p;
    }
    return NULL;
}

static BYTE *put_abs_jmp(BYTE *p, const void *to)
{
    p[0] = 0xff; p[1] = 0x25; memset(p + 2, 0, 4); memcpy(p + 6, &to, 8);
    return p + 14;
}

/* the length of the export's first instruction, and its RIP-relative displacement's offset in it
 * (0: none), for what a builtin export can start with */
static int insn_len(const BYTE *p, int *disp_at)
{
    *disp_at = 0;
    if (p[0] == 0xff && p[1] == 0x25) { *disp_at = 2; return 6; }  /* jmp [rip+disp32] */
    if (p[0] == 0x0f && p[1] == 0x0b) return 2;                    /* ud2 */
    return -1;
}

struct hook { BYTE *target, saved[16], *page; int len; };

static int hook_abs14(struct hook *h, BYTE *target, void *detour, void **orig)
{
    DWORD old;
    h->target = target; h->len = 14; memcpy(h->saved, target, 14);
    if (!(h->page = VirtualAlloc(NULL, 4096, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE))) return 0;
    memcpy(h->page, target, 14);              /* copied whole: jmp [rip+0] reads the copied address */
    put_abs_jmp(h->page + 14, target + 14);
    *orig = h->page;
    if (!VirtualProtect(target, 14, PAGE_EXECUTE_READWRITE, &old)) return 0;
    put_abs_jmp(target, detour);
    VirtualProtect(target, 14, old, &old);
    FlushInstructionCache(GetCurrentProcess(), target, 14);
    return 1;
}

static int hook_rel5(struct hook *h, BYTE *target, void *detour, void **orig)
{
    DWORD old;
    int disp_at, n = 0, len;
    BYTE *relay, *tramp;
    INT32 rel;
    while (n < 5)
    {
        if ((len = insn_len(target + n, &disp_at)) < 0) { printf("hooktest: unknown prologue %02x %02x\n", target[n], target[n + 1]); return 0; }
        n += len;
        if (disp_at && n < 5) return 0;
    }
    h->target = target; h->len = 5; memcpy(h->saved, target, 5);
    if (!(h->page = alloc_near(target))) return 0;
    relay = h->page; tramp = h->page + 32;
    put_abs_jmp(relay, detour);
    memcpy(tramp, target, n);
    insn_len(target, &disp_at);
    if (disp_at)
    {
        /* re-aim the displacement at what the original instruction reached */
        INT32 d; memcpy(&d, target + disp_at, 4);
        BYTE *abs = target + n + d;           /* (one instruction: its end is n) */
        d = (INT32)(abs - (tramp + n));
        memcpy(tramp + disp_at, &d, 4);
    }
    put_abs_jmp(tramp + n, target + n);
    *orig = tramp;
    if (!VirtualProtect(target, 5, PAGE_EXECUTE_READWRITE, &old)) return 0;
    rel = (INT32)(relay - (target + 5));
    target[0] = 0xe9; memcpy(target + 1, &rel, 4);
    VirtualProtect(target, 5, old, &old);
    FlushInstructionCache(GetCurrentProcess(), target, 5);
    return 1;
}

static void unhook(struct hook *h)
{
    DWORD old;
    VirtualProtect(h->target, h->len, PAGE_EXECUTE_READWRITE, &old);
    memcpy(h->target, h->saved, h->len);
    VirtualProtect(h->target, h->len, old, &old);
    FlushInstructionCache(GetCurrentProcess(), h->target, h->len);
}

int main(void)
{
    BYTE *gsm = (BYTE *)GetProcAddress(GetModuleHandleA("user32.dll"), "GetSystemMetrics");
    BYTE *gcpi = (BYTE *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "GetCurrentProcessId");
    int (WINAPI *p_gsm)(int) = (void *)gsm;
    DWORD (WINAPI *p_gcpi)(void) = (void *)gcpi;
    int want = GetSystemMetrics(SM_CXSCREEN), r, k;
    DWORD pid = GetCurrentProcessId();
    struct hook h;
    static const char *how[2] = { "abs14", "rel5" };

    printf("hooktest: GetSystemMetrics at %p: %02x %02x %02x %02x %02x %02x, SM_CXSCREEN %d\n",
           gsm, gsm[0], gsm[1], gsm[2], gsm[3], gsm[4], gsm[5], want);
    printf("hooktest: GetCurrentProcessId at %p: %02x %02x, pid %lu\n", gcpi, gcpi[0], gcpi[1], pid);
    for (k = 0; k < 2; k++)
    {
        char name[64]; int f0 = failures;
        hits = 0;
        if (!(k ? hook_rel5 : hook_abs14)(&h, gsm, hook_gsm, (void **)&orig_gsm)) { printf("hooktest FAIL %s user32: cannot hook\n", how[k]); failures++; continue; }
        r = p_gsm(SM_CXSCREEN);
        snprintf(name, sizeof(name), "%s user32 pointer (%d, %d hits)", how[k], r, hits); check(name, r == want && hits == 1);
        r = GetSystemMetrics(SM_CXSCREEN);
        snprintf(name, sizeof(name), "%s user32 import (%d, %d hits)", how[k], r, hits); check(name, r == want && hits == 2);
        unhook(&h);
        r = p_gsm(SM_CXSCREEN);
        snprintf(name, sizeof(name), "%s user32 unhooked (%d, %d hits)", how[k], r, hits); check(name, r == want && hits == 2);
        if (failures == f0) printf("hooktest %s user32 ok\n", how[k]);

        f0 = failures; hits = 0;
        if (!(k ? hook_rel5 : hook_abs14)(&h, gcpi, hook_gcpi, (void **)&orig_gcpi)) { printf("hooktest FAIL %s kernelbase: cannot hook\n", how[k]); failures++; continue; }
        snprintf(name, sizeof(name), "%s kernelbase pointer (%d hits)", how[k], hits); check(name, p_gcpi() == pid && hits == 1);
        snprintf(name, sizeof(name), "%s kernelbase import (%d hits)", how[k], hits); check(name, GetCurrentProcessId() == pid && hits == 2);
        unhook(&h);
        snprintf(name, sizeof(name), "%s kernelbase unhooked (%d hits)", how[k], hits); check(name, p_gcpi() == pid && hits == 2);
        if (failures == f0) printf("hooktest %s kernelbase ok\n", how[k]);
    }
    printf("hooktest: %d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
