/* A 64-bit parent reaching a child's memory above 4 GiB, the way a
 * sandboxing parent (Chromium's, in Firefox and Thunderbird) readies its
 * child: the child starts suspended, the parent reads its PEB, then the
 * image header at the PEB's image base (0x140000000), then makes that
 * page writable and writes and reads back its DOS stub (as the sandbox
 * patches the child's ntdll), and writes and reads a page it allocates. Each check prints one
 * line; the child, resumed, returns 0x42.
 */
#include <windows.h>
#include <winternl.h>
#include <stdio.h>
#include <string.h>

static int fails;
static void check(int ok, const char *what)
{
    printf("%s %s\n", ok ? "ok  " : "FAIL", what);
    fflush(stdout);
    fails += !ok;
}

int main(int argc, char **argv)
{
    char cmd[MAX_PATH + 16], mz[2] = { 0 }, got[16] = { 0 };
    static const char text[16] = "peerhigh data";
    STARTUPINFOA si = { sizeof(si) };
    PROCESS_INFORMATION pi;
    PROCESS_BASIC_INFORMATION pbi;
    void *peb[6] = { 0 }, *image, *page;
    SIZE_T n = 0;
    DWORD code = 0, old;

    if (argc > 1 && !strcmp(argv[1], "child")) return 0x42;
    sprintf(cmd, "\"%s\" child", argv[0]);
    if (!CreateProcessA(NULL, cmd, NULL, NULL, FALSE, CREATE_SUSPENDED, NULL, NULL, &si, &pi)) {
        printf("FAIL spawn %lu\n", GetLastError());
        return 1;
    }
    check(!NtQueryInformationProcess(pi.hProcess, ProcessBasicInformation, &pbi, sizeof(pbi), NULL), "find the PEB");
    check(ReadProcessMemory(pi.hProcess, pbi.PebBaseAddress, peb, sizeof(peb), &n) && n == sizeof(peb), "read the PEB");
    image = peb[2];
    printf("     image base %p\n", image);
    n = 0;
    check(ReadProcessMemory(pi.hProcess, image, mz, sizeof(mz), &n) && n == 2 && mz[0] == 'M' && mz[1] == 'Z',
          "read the image header");
    if (VirtualProtectEx(pi.hProcess, image, 4096, PAGE_READWRITE, &old)) {
        check(WriteProcessMemory(pi.hProcess, (char *)image + 0x4e, text, sizeof(text), &n) && n == sizeof(text),
              "write over the image header");
        memset(got, 0, sizeof(got));
        check(ReadProcessMemory(pi.hProcess, (char *)image + 0x4e, got, sizeof(got), &n) && n == sizeof(got) &&
              !strcmp(got, text), "read it back");
    } else check(0, "make the image header writable");
    page = VirtualAllocEx(pi.hProcess, NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    printf("     page %p\n", page);
    check(page != NULL, "allocate a page");
    if (page) {
        check(WriteProcessMemory(pi.hProcess, page, text, sizeof(text), &n) && n == sizeof(text), "write the page");
        memset(got, 0, sizeof(got));
        check(ReadProcessMemory(pi.hProcess, page, got, sizeof(got), &n) && n == sizeof(got) && !strcmp(got, text),
              "read the page back");
    }
    ResumeThread(pi.hThread);
    WaitForSingleObject(pi.hProcess, 120000);
    GetExitCodeProcess(pi.hProcess, &code);
    check(code == 0x42, "the child runs once resumed");
    printf("peerhigh done, %d failed\n", fails);
    return fails;
}
