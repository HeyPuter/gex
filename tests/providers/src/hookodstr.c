/* A Detours-style hook on a provided export: the guest overwrites the start
 * of kernel32's OutputDebugStringA with a `jmp` to its own function, calls
 * it, then puts the bytes back. On Windows the hook runs. With the
 * debug_string provider bound there it does not: the provider serves the
 * export's address and the rewritten bytes never run (merge/eject9's
 * "providers survive library rewrites"), and gex logs it once per export
 * ("gex-nt: ... rewrote provided export"). tests/providers/run.sh checks
 * both outputs and the log line. Built with i686-w64-mingw32-gcc -O2 -s. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static volatile int hook_calls;
static void WINAPI hook(LPCSTR s) { (void)s; hook_calls++; }

int main(void) {
    typedef void (WINAPI *odsa_t)(LPCSTR);
    unsigned char *p = (unsigned char *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "OutputDebugStringA");
    odsa_t odsa = (odsa_t)p;
    unsigned char saved[5], jmp[5] = {0xe9};
    DWORD old;
    LONG rel = (LONG)((unsigned char *)hook - (p + 5));
    memcpy(jmp + 1, &rel, 4);
    memcpy(saved, p, 5);
    if (!VirtualProtect(p, 5, PAGE_EXECUTE_READWRITE, &old)) return 2;
    memcpy(p, jmp, 5);
    VirtualProtect(p, 5, old, &old);
    FlushInstructionCache(GetCurrentProcess(), p, 5);
    odsa("hooked\n");
    printf("hook calls while hooked: %d\n", hook_calls);
    VirtualProtect(p, 5, PAGE_EXECUTE_READWRITE, &old);
    memcpy(p, saved, 5);
    VirtualProtect(p, 5, old, &old);
    FlushInstructionCache(GetCurrentProcess(), p, 5);
    odsa("unhooked\n");
    printf("hook calls after unhooking: %d\n", hook_calls);
    return 0;
}
