#include <windows.h>

static HANDLE ready;
static volatile DWORD observed;
static DWORD worker(void *argument) {
    Sleep(5);
    observed = (DWORD)(ULONG_PTR)argument;
    SetEvent(ready);
    return 42;
}
static DWORD (*volatile callback)(void *) = worker;
int entry(void) {
    ready = CreateEventW(0, FALSE, FALSE, 0);
    HANDLE thread = CreateThread(0, 0, callback, (void *)123, 0, 0);
    if (!ready || !thread) {
        return 1;
    }
    if (WaitForSingleObject(ready, 1000) != WAIT_OBJECT_0 || observed != 123) {
        return 2;
    }
    if (WaitForSingleObject(thread, 1000) != WAIT_OBJECT_0) {
        return 3;
    }
    DWORD code;
    if (!GetExitCodeThread(thread, &code) || code != 42) {
        return 4;
    }
    CloseHandle(thread);
    CloseHandle(ready);
    return 0;
}
