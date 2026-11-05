/* Threads made and joined one after another, more in all than a process
 * has register-context slots and more than its address space holds
 * provider stacks for: each that ends gives both back for the next.
 *
 * Prints "thrchurn ok N" and exits 0. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static DWORD WINAPI proc(void *p)
{
    char buf[MAX_PATH];
    /* into native code, so the thread has its provider stacks */
    return GetTempPathA(sizeof(buf), buf) ? 0 : 1;
}

int main(int argc, char **argv)
{
    int n = argc > 1 ? atoi(argv[1]) : 1500, i;
    DWORD code;

    for (i = 0; i < n; i++)
    {
        HANDLE h = CreateThread(NULL, 0, proc, NULL, 0, NULL);
        if (!h)
        {
            printf("CreateThread %d failed: %lu\n", i, GetLastError());
            return 1;
        }
        WaitForSingleObject(h, INFINITE);
        GetExitCodeThread(h, &code);
        CloseHandle(h);
        if (code)
        {
            printf("thread %d exited with %lu\n", i, code);
            return 1;
        }
    }
    printf("thrchurn ok %d\n", n);
    return 0;
}
