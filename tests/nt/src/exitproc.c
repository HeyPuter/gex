/* ExitProcess from a secondary thread while the main thread waits: the
 * process ends with that thread's code, after atexit handlers run.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

static void on_exit(void) { printf("atexit ran\n"); fflush(stdout); }
static DWORD WINAPI worker(void *p) {
    Sleep(20);
    printf("worker exiting\n");
    fflush(stdout);
    exit(33);
}
int main(void) {
    atexit(on_exit);
    CreateThread(0, 0, worker, 0, 0, 0);
    WaitForSingleObject(CreateEventW(0, TRUE, FALSE, 0), INFINITE);
    printf("FAIL main woke\n");
    return 1;
}
