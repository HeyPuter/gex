/* gex-nt-arch: x64
 * gex-nt-libs: -lopengl32 -lgdi32 */
/* The 64-bit side of run.sh's mixed rows (x86_64-w64-mingw32):
 *
 *   child         exit 64 (wow32 spawn64's child)
 *   spawn32 EXE   CreateProcess of a 32-bit program's "child" case: exit 32,
 *                 IsWow64Process TRUE for it, and FALSE for this one
 *   xmmcb         xmm6-xmm15 (callee-saved) kept across SendMessage whose
 *                 window procedure, a user callback, changes them
 *   maskmov       maskmovdqu: the masked bytes only, to [rdi]
 *   sysarch       GetSystemInfo and GetNativeSystemInfo both AMD64
 *   genrandom     rand_s and advapi32's SystemFunction036 (RtlGenRandom)
 *                 in a process cryptbase is not loaded in: both reach
 *                 cryptbase's, which advapi32 only forwards to
 *   sysbench N    N calls of NtQueryPerformanceCounter, then of
 *                 NtQueryVirtualMemory: the 64-bit baseline of wow32's
 *                 sysbench (ns per call)
 *
 * Prints "ok   ..." lines and "wow64c CASE ok", or "FAIL ..." and exits 1. */
#define _CRT_RAND_S
#include <windows.h>
#include <setupapi.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

/* the window procedure for xmmcb, in assembly so that nothing saves the
 * registers it clobbers: xmm6-15 are the callee's to keep, and this one
 * breaks that on purpose, as a callback that ends by an unwind would; the
 * dispatcher must still hand the caller its own values back */
LRESULT CALLBACK clobber_proc(HWND h, UINT m, WPARAM w, LPARAM l);
LONG clobber_calls;
__asm__(".text\n\t.globl clobber_proc\n"
        "clobber_proc:\n\t"
        "cmpl $0x400, %edx\n\t"
        "jne 1f\n\t"
        "lock incl clobber_calls(%rip)\n\t"
        "pcmpeqd %xmm6, %xmm6\n\t"
        "pcmpeqd %xmm9, %xmm9\n\t"
        "pcmpeqd %xmm15, %xmm15\n\t"
        "movl $7, %eax\n\t"
        "ret\n"
        "1:\tjmp *__imp_DefWindowProcA(%rip)");

static int case_xmmcb(void)
{
    static const ULONG64 pat[2] = { 0x0123456789abcdefull, 0xfedcba9876543210ull };
    WNDCLASSA wc = { 0 };
    ULONG64 out[3][2];
    HWND h;
    DWORD_PTR r = 0;

    wc.lpfnWndProc = clobber_proc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "wow64c_xmmcb";
    RegisterClassA(&wc);
    h = CreateWindowA("wow64c_xmmcb", "", 0, 0, 0, 0, 0, HWND_MESSAGE, NULL, wc.hInstance, NULL);
    if (!h) { printf("FAIL xmmcb CreateWindow\n"); return 1; }
    /* the values live only in xmm6, xmm9 and xmm15 across the call;
     * SendMessageTimeout reaches the procedure through a user-mode callback
     * from win32u (SendMessage would call it from user32 itself) */
    __asm__ volatile(
        "movdqu (%[pat]), %%xmm6\n\t"
        "movdqu (%[pat]), %%xmm9\n\t"
        "movdqu (%[pat]), %%xmm15\n\t"
        "pushq %%rbx\n\t"
        "movq %%rsp, %%rbx\n\t"
        "andq $-16, %%rsp\n\t"
        "subq $64, %%rsp\n\t"
        "movq %[h], %%rcx\n\t"
        "movl $0x400, %%edx\n\t"
        "xorl %%r8d, %%r8d\n\t"
        "xorl %%r9d, %%r9d\n\t"
        "movq $0, 32(%%rsp)\n\t"      /* SMTO_NORMAL */
        "movq $5000, 40(%%rsp)\n\t"   /* timeout */
        "movq %[res], 48(%%rsp)\n\t"
        "call *%[fn]\n\t"
        "movq %%rbx, %%rsp\n\t"
        "popq %%rbx\n\t"
        "movdqu %%xmm6, (%[out])\n\t"
        "movdqu %%xmm9, 16(%[out])\n\t"
        "movdqu %%xmm15, 32(%[out])"
        :
        : [pat] "r"(pat), [h] "r"(h), [fn] "r"(SendMessageTimeoutA), [out] "r"(out), [res] "r"(&r)
        : "rax", "rbx", "rcx", "rdx", "r8", "r9", "r10", "r11", "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5",
          "xmm6", "xmm9", "xmm15", "memory");
    DestroyWindow(h);
    if (r != 7 || memcmp(out[0], pat, 16) || memcmp(out[1], pat, 16) || memcmp(out[2], pat, 16))
    {
        printf("FAIL xmmcb result %ld (procedure ran %ld times), xmm6 %016llx%016llx xmm9 %016llx%016llx xmm15 %016llx%016llx\n", (long)r, (long)clobber_calls,
               out[0][1], out[0][0], out[1][1], out[1][0], out[2][1], out[2][0]);
        return 1;
    }
    printf("ok   xmmcb xmm6, xmm9 and xmm15 kept across a callback\n");
    printf("wow64c xmmcb ok\n");
    return 0;
}

static int case_maskmov(void)
{
    static const unsigned char src[16] = { 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16 };
    static const unsigned char mask[16] = { 0x80, 0, 0xff, 0x7f, 0, 0x81, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x90 };
    unsigned char dst[24];
    int k, bad = 0;

    memset(dst, 0xee, sizeof(dst));
    __asm__ volatile("movdqu (%0), %%xmm0\n\tmovdqu (%1), %%xmm1\n\tmaskmovdqu %%xmm1, %%xmm0"
                     :: "r"(src), "r"(mask), "D"(dst + 4) : "xmm0", "xmm1", "memory");
    for (k = 0; k < 24; k++)
    {
        int i = k - 4;
        unsigned char want = i >= 0 && i < 16 && (mask[i] & 0x80) ? src[i] : 0xee;
        if (dst[k] != want) bad++;
    }
    if (bad) { printf("FAIL maskmov maskmovdqu %d wrong bytes\n", bad); return 1; }
    printf("ok   maskmov maskmovdqu\n");
    printf("wow64c maskmov ok\n");
    return 0;
}

static int case_sysarch(void)
{
    SYSTEM_INFO si, ni;

    GetSystemInfo(&si);
    GetNativeSystemInfo(&ni);
    if (si.wProcessorArchitecture != PROCESSOR_ARCHITECTURE_AMD64 || ni.wProcessorArchitecture != PROCESSOR_ARCHITECTURE_AMD64)
    {
        printf("FAIL sysarch GetSystemInfo %u GetNativeSystemInfo %u\n", si.wProcessorArchitecture, ni.wProcessorArchitecture);
        return 1;
    }
    printf("ok   sysarch GetSystemInfo and GetNativeSystemInfo AMD64\n");
    printf("wow64c sysarch ok\n");
    return 0;
}

/* setupapi's fake DLLs, as wineboot's prefix update installs them: a
 * WineFakeDlls entry for explorer.exe must leave a file behind (a setupapi
 * that cannot find the DLL in its own arch's directory deletes it instead) */
static int case_fakedll(void)
{
    void *(WINAPI *open_inf)(const char *, const char *, DWORD, UINT *);
    BOOL (WINAPI *install)(HWND, void *, const char *, UINT, HKEY, const char *, UINT, void *, void *, HDEVINFO, void *);
    void (WINAPI *close_inf)(void *);
    HMODULE setupapi = LoadLibraryA("setupapi.dll");
    char dir[MAX_PATH], inf[MAX_PATH], dll[MAX_PATH];
    WIN32_FILE_ATTRIBUTE_DATA fa;
    void *h;
    HANDLE f;
    DWORD n;
    BOOL ok;
    static const char text[] =
        "[Version]\r\nSignature=\"$Chicago$\"\r\n"
        "[DefaultInstall]\r\nWineFakeDlls=Fake\r\n"
        "[Fake]\r\n11,gexfake,explorer.exe\r\n";

    open_inf = (void *)GetProcAddress(setupapi, "SetupOpenInfFileA");
    install = (void *)GetProcAddress(setupapi, "SetupInstallFromInfSectionA");
    close_inf = (void *)GetProcAddress(setupapi, "SetupCloseInfFile");
    if (!open_inf || !install || !close_inf) { printf("FAIL fakedll no setupapi\n"); return 1; }
    GetTempPathA(sizeof(dir), dir);
    sprintf(inf, "%sgexfake.inf", dir);
    f = CreateFileA(inf, GENERIC_WRITE, 0, NULL, CREATE_ALWAYS, 0, NULL);
    WriteFile(f, text, sizeof(text) - 1, &n, NULL);
    CloseHandle(f);
    GetSystemDirectoryA(dir, sizeof(dir));
    sprintf(dll, "%s\\gexfake\\explorer.exe", dir);
    if ((h = open_inf(inf, NULL, INF_STYLE_WIN4, NULL)) == INVALID_HANDLE_VALUE) { printf("FAIL fakedll SetupOpenInfFile %lu\n", GetLastError()); return 1; }
    ok = install(NULL, h, "DefaultInstall", SPINST_ALL, NULL, NULL, 0, NULL, NULL, NULL, NULL);
    close_inf(h);
    DeleteFileA(inf);
    if (!ok) { printf("FAIL fakedll SetupInstallFromInfSection %lu\n", GetLastError()); return 1; }
    if (!GetFileAttributesExA(dll, GetFileExInfoStandard, &fa) || !fa.nFileSizeLow)
    {
        printf("FAIL fakedll %s missing after install (%lu)\n", dll, GetLastError());
        return 1;
    }
    printf("ok   fakedll %s, %lu bytes\n", dll, fa.nFileSizeLow);
    DeleteFileA(dll);
    dll[strlen(dll) - strlen("\\explorer.exe")] = 0;
    RemoveDirectoryA(dll);
    printf("wow64c fakedll ok\n");
    return 0;
}

/* a child in a console of its own (CREATE_NEW_CONSOLE): its conhost has to
 * come up, so the child's console calls work */
static int case_newconsole(void)
{
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    char cmd[MAX_PATH + 16];
    char self[MAX_PATH];
    DWORD code = 99;

    GetModuleFileNameA(NULL, self, sizeof(self));
    sprintf(cmd, "\"%s\" conchild", self);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, CREATE_NEW_CONSOLE, NULL, NULL, &si, &pi))
    {
        printf("FAIL newconsole CreateProcess %lu\n", GetLastError());
        return 1;
    }
    if (WaitForSingleObject(pi.hProcess, 120000)) { printf("FAIL newconsole child hangs\n"); TerminateProcess(pi.hProcess, 1); return 1; }
    GetExitCodeProcess(pi.hProcess, &code);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    if (code) { printf("FAIL newconsole child: %s\n", code == 2 ? "no CONOUT$" : code == 3 ? "GetConsoleScreenBufferInfo failed" : "exit code"); return 1; }
    printf("ok   newconsole GetConsoleScreenBufferInfo in the new console\n");
    printf("wow64c newconsole ok\n");
    return 0;
}

static int conchild(void)
{
    CONSOLE_SCREEN_BUFFER_INFO info;
    HANDLE out = CreateFileA("CONOUT$", GENERIC_READ | GENERIC_WRITE, FILE_SHARE_READ | FILE_SHARE_WRITE, NULL, OPEN_EXISTING, 0, NULL);

    if (out == INVALID_HANDLE_VALUE) return 2;
    if (!GetConsoleScreenBufferInfo(out, &info) || !info.dwSize.X || !info.dwSize.Y) return 3;
    return 0;
}

static int case_genrandom(void)
{
    BOOLEAN (WINAPI *gen)(void *, ULONG);
    unsigned char buf[16] = { 0 }, zero[16] = { 0 };
    unsigned int r = 0;

    if (GetModuleHandleA("cryptbase.dll"))
    {
        printf("FAIL genrandom cryptbase.dll already loaded at start\n");
        return 1;
    }
    /* the C runtime's call: its delay import of advapi32, whose export is cryptbase's */
    if (rand_s(&r))
    {
        printf("FAIL genrandom rand_s failed\n");
        return 1;
    }
    printf("ok   genrandom rand_s\n");
    gen = (void *)GetProcAddress(LoadLibraryA("advapi32.dll"), "SystemFunction036");
    if (!gen || !gen(buf, sizeof(buf)) || !memcmp(buf, zero, sizeof(buf)))
    {
        printf("FAIL genrandom advapi32 SystemFunction036 %p\n", gen);
        return 1;
    }
    printf("ok   genrandom advapi32 SystemFunction036, cryptbase %s\n", GetModuleHandleA("cryptbase.dll") ? "loaded" : "not loaded");
    printf("wow64c genrandom ok\n");
    return 0;
}

int main(int argc, char **argv)
{
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    DWORD code = 0;
    BOOL wow = FALSE, self = TRUE;
    char cmd[MAX_PATH + 16];

    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && !strcmp(argv[1], "child")) return 64;
    if (argc > 1 && !strcmp(argv[1], "xmmcb")) return case_xmmcb();
    if (argc > 1 && !strcmp(argv[1], "sysarch")) return case_sysarch();
    if (argc > 1 && !strcmp(argv[1], "fakedll")) return case_fakedll();
    if (argc > 1 && !strcmp(argv[1], "newconsole")) return case_newconsole();
    if (argc > 1 && !strcmp(argv[1], "conchild")) return conchild();
    if (argc > 1 && !strcmp(argv[1], "genrandom")) return case_genrandom();
    if (argc > 1 && !strcmp(argv[1], "maskmov")) return case_maskmov();
    if (argc > 1 && !strcmp(argv[1], "sysbench"))
    {
        LONG (WINAPI *qpc)(LARGE_INTEGER *, LARGE_INTEGER *) =
            (void *)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtQueryPerformanceCounter");
        LARGE_INTEGER c, f, t0, t1;
        int i, n = argc > 2 ? atoi(argv[2]) : 100000;
        LONG st = 0;

        QueryPerformanceFrequency(&f);
        QueryPerformanceCounter(&t0);
        for (i = 0; i < n; i++) st |= qpc(&c, NULL);
        QueryPerformanceCounter(&t1);
        if (st) { printf("FAIL sysbench status %lx\n", st); return 1; }
        printf("ok   sysbench %d calls: %.1f ns per call (%.3f s)\n", n,
               (double)(t1.QuadPart - t0.QuadPart) * 1e9 / f.QuadPart / n,
               (double)(t1.QuadPart - t0.QuadPart) / f.QuadPart);
        {
            LONG (WINAPI *qvm)(HANDLE, const void *, ULONG, void *, SIZE_T, SIZE_T *) =
                (void *)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtQueryVirtualMemory");
            MEMORY_BASIC_INFORMATION mbi;

            QueryPerformanceCounter(&t0);
            for (i = 0; i < n; i++) st |= qvm(GetCurrentProcess(), &mbi, 0, &mbi, sizeof(mbi), NULL);
            QueryPerformanceCounter(&t1);
            if (st) { printf("FAIL sysbench NtQueryVirtualMemory status %lx\n", st); return 1; }
            printf("ok   sysbench %d NtQueryVirtualMemory calls: %.1f ns per call (%.3f s)\n", n,
                   (double)(t1.QuadPart - t0.QuadPart) * 1e9 / f.QuadPart / n,
                   (double)(t1.QuadPart - t0.QuadPart) / f.QuadPart);
        }
        printf("wow64c sysbench ok\n");
        return 0;
    }
    if (argc < 3 || strcmp(argv[1], "spawn32")) { printf("FAIL usage\n"); return 1; }
    snprintf(cmd, sizeof(cmd), "\"%s\" child", argv[2]);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi))
    {
        printf("FAIL spawn32 CreateProcess %s: error %lu\n", argv[2], GetLastError());
        return 1;
    }
    WaitForSingleObject(pi.hProcess, 120000);
    GetExitCodeProcess(pi.hProcess, &code);
    IsWow64Process(pi.hProcess, &wow);
    IsWow64Process(GetCurrentProcess(), &self);
    CloseHandle(pi.hThread);
    CloseHandle(pi.hProcess);
    if (code != 32 || !wow || self)
    {
        printf("FAIL spawn32 32-bit child exit %lu, IsWow64Process(child) %d, (self) %d\n", code, wow, self);
        return 1;
    }
    printf("ok   spawn32 32-bit child exit %lu, IsWow64Process(child) %d\n", code, wow);
    printf("wow64c spawn32 ok\n");
    return 0;
}
