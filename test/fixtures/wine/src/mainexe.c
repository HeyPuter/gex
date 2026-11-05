/* An x64 program whose preferred base (0x140000000, mingw's default) 64-bit Wine
 * cannot reach starts as the first process, relocated, not through
 * start.exe /exec: no start.exe process runs, its image is where the
 * loader placed it, and its relocated data and code work.
 * gex-nt-arch: x64 */
#include <windows.h>
#include <tlhelp32.h>
#include <stdio.h>

static int fail;
static void check(int ok, const char *what)
{
    printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) fail++;
}

static int twice(int x) { return 2 * x; }
static int (*volatile fn)(int) = twice;  /* a pointer the relocations fix */
static const char *volatile msg = "relocated";

int main(void)
{
    HMODULE self = GetModuleHandleW(NULL);
    IMAGE_NT_HEADERS *nt = (IMAGE_NT_HEADERS *)((char *)self + ((IMAGE_DOS_HEADER *)self)->e_lfanew);
    PROCESSENTRY32W pe = { sizeof(pe) };
    HANDLE snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    int starts = 0;
    char buf[160];

    if (snap != INVALID_HANDLE_VALUE && Process32FirstW(snap, &pe))
        do if (!lstrcmpiW(pe.szExeFile, L"start.exe")) starts++; while (Process32NextW(snap, &pe));
    if (snap != INVALID_HANDLE_VALUE) CloseHandle(snap);
    check(snap != INVALID_HANDLE_VALUE, "the process list is readable");
    sprintf(buf, "no start.exe process (%d)", starts);
    check(!starts, buf);
    sprintf(buf, "the image at %p, not its preferred base %#llx", (void *)self, (unsigned long long)nt->OptionalHeader.ImageBase);
    check((ULONG_PTR)self != nt->OptionalHeader.ImageBase, buf);
    check(fn(21) == 42, "a relocated function pointer");
    check(!lstrcmpA(msg, "relocated"), "a relocated string pointer");
    if (!fail) printf("mainexe ok\n");
    return fail;
}
