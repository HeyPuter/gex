/* Child processes one after another, each a new address space: a child
 * whose memory lands where an exited one's had been must still get its
 * native modules' data relocated (gex once skipped it, keyed by the
 * memory's host address: a child then saw ntdll's lists as small offsets).
 * Each child registers a DLL notification and loads a DLL (ntdll's loader
 * lists and the notification list are relocated data). Native Wine (W3)
 * only: GEX_NT_CHILDREN=1 runs it.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <string.h>
typedef LONG (WINAPI *reg_t)(ULONG, void *, void *, void **);
static int calls;
static void CALLBACK note(ULONG reason, const void *data, void *ctx) { if (ctx == (void *)0x1234) calls++; }
static int child(void) {
    void *cookie = 0;
    reg_t reg = (reg_t)GetProcAddress(GetModuleHandleA("ntdll.dll"), "LdrRegisterDllNotification");
    if (!reg || reg(0, (void *)note, (void *)0x1234, &cookie)) return 2;
    if (!LoadLibraryA("version.dll")) return 3;
    return calls ? 0 : 4;
}
int main(int argc, char **argv) {
    char cmd[MAX_PATH + 16], self[MAX_PATH];
    int k, bad = 0;
    if (argc > 1 && !strcmp(argv[1], "child")) return child();
    GetModuleFileNameA(0, self, sizeof self);
    for (k = 0; k < 8; k++) {
        STARTUPINFOA si = {sizeof si};
        PROCESS_INFORMATION pi;
        DWORD code = 99;
        snprintf(cmd, sizeof cmd, "\"%s\" child", self);
        if (!CreateProcessA(0, cmd, 0, 0, TRUE, 0, 0, 0, &si, &pi)) return 98;
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &code);
        if (code) { printf("child %d exit %lu\n", k, code); bad++; }
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    printf("%d of 8 children failed\n", bad);
    return bad != 0;
}
