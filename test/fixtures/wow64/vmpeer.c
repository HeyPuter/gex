/* vmpeer: ReadProcessMemory and WriteProcessMemory on a second process,
 * a copy of this program waiting on an event. Each check prints one line;
 * the child's exit code says what it saw: 0x50 for the written marker,
 * plus what its code at peek returns (7 as built, 5 once patched).
 * "vmpeer crash": an illegal instruction nothing handles, for the
 * system's debugger to attach to. "vmpeer quick": the same checks with no
 * settling sleep, made while the child is still starting. */
#include <windows.h>
#include <winternl.h>
#include <stdio.h>
#include <string.h>

int peek(void);
__asm__(".text\n.globl _peek\n.p2align 4\n_peek:\n movl $7, %eax\n ret\n .fill 26, 1, 0x90\n");

static volatile char shared_buf[64] = "vmpeer child data";
static volatile DWORD marker;

static int child(const char *ev)
{
    HANDLE go = OpenEventA(SYNCHRONIZE, FALSE, ev);
    if (!go) return 2;
    WaitForSingleObject(go, 120000);
    return (marker == 0x5eed1234 ? 0x50 : 0x30) | peek();
}

static int fails;
static void check(int ok, const char *what)
{
    printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    fflush(stdout);
    fails += !ok;
}

typedef NTSTATUS (WINAPI *write_vm_fn)(HANDLE, void *, const void *, SIZE_T, SIZE_T *);

int main(int argc, char **argv)
{
    char cmd[MAX_PATH + 64], ev[32], got[64];
    static char big[8192];
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    SIZE_T n = 0;
    DWORD code = 0, m = 0x5eed1234, old;
    static const unsigned char ret5[] = { 0xb8, 5, 0, 0, 0, 0xc3 };
    write_vm_fn write_vm = (write_vm_fn)GetProcAddress(GetModuleHandleA("ntdll.dll"), "NtWriteVirtualMemory");
    HANDLE go;
    char *pages;
    NTSTATUS st;

    if (argc > 2 && !strcmp(argv[1], "child")) return child(argv[2]);
    if (argc > 1 && !strcmp(argv[1], "crash")) {
        printf("vmpeer crash\n");
        fflush(stdout);
        __asm__ volatile("ud2");
        return 0;
    }
    sprintf(ev, "vmpeer-%lu", GetCurrentProcessId());
    go = CreateEventA(NULL, TRUE, FALSE, ev);
    sprintf(cmd, "\"%s\" child %s", argv[0], ev);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, 0, NULL, NULL, &si, &pi)) {
        printf("FAIL spawn %lu\n", GetLastError());
        return 1;
    }
    if (argc < 2 || strcmp(argv[1], "quick")) Sleep(1000);
    memset(got, 0, sizeof(got));
    check(ReadProcessMemory(pi.hProcess, (void *)shared_buf, got, sizeof(got), &n) && n == sizeof(got) &&
              !strcmp(got, "vmpeer child data"), "read a global");
    check(WriteProcessMemory(pi.hProcess, (void *)&marker, &m, sizeof(m), &n) && n == sizeof(m), "write a global");
    check(WriteProcessMemory(pi.hProcess, (void *)peek, ret5, sizeof(ret5), &n) && n == sizeof(ret5), "write over code");
    pages = VirtualAllocEx(pi.hProcess, NULL, 8192, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    check(pages != NULL, "allocate two pages");
    if (pages) {
        check(VirtualFreeEx(pi.hProcess, pages + 4096, 4096, MEM_DECOMMIT), "decommit the second");
        n = 0;
        check(!ReadProcessMemory(pi.hProcess, pages, big, sizeof(big), &n) && GetLastError() == ERROR_PARTIAL_COPY,
              "a read across them is a partial copy");
        printf("     partial read: %lu bytes\n", (unsigned long)n);
        check(VirtualProtectEx(pi.hProcess, pages, 4096, PAGE_READONLY, &old), "make the first read-only");
        n = 1;
        st = write_vm(pi.hProcess, pages, &m, sizeof(m), &n);
        check(st != 0, "NtWriteVirtualMemory to a read-only page fails");
        printf("     status %08lx, %lu bytes\n", (unsigned long)st, (unsigned long)n);
    }
    SetEvent(go);
    WaitForSingleObject(pi.hProcess, 120000);
    GetExitCodeProcess(pi.hProcess, &code);
    printf("child exit %02lx\n", (unsigned long)code);
    check((code & 0xf0) == 0x50, "the child saw the written marker");
    check(!ReadProcessMemory(pi.hProcess, (void *)shared_buf, got, 4, &n), "a read of an exited process fails");
    printf("vmpeer done, %d failed\n", fails);
    return fails;
}
