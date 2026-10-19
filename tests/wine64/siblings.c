/* Each argument a command line, run as a child process in turn, waited for,
 * and its exit code printed ("child exit N"); exits 0. The children are
 * siblings: the memory flavour one of them gets (gex docs/mem64.md §5, its
 * image's record) must not reach the next.
 *
 *   x86_64-w64-mingw32-gcc -O1 -o siblings.exe siblings.c */
#include <windows.h>
#include <stdio.h>

int main(int argc, char **argv)
{
    for (int i = 1; i < argc; i++)
    {
        STARTUPINFOA si = { sizeof si };
        PROCESS_INFORMATION pi;
        DWORD code = 99;
        fflush(stdout);
        if (!CreateProcessA(NULL, argv[i], NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi))
        {
            printf("FAIL CreateProcess %lu\n", GetLastError());
            continue;
        }
        WaitForSingleObject(pi.hProcess, INFINITE);
        GetExitCodeProcess(pi.hProcess, &code);
        printf("child exit %lu\n", code);
    }
    return 0;
}
