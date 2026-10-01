/* User APCs: queued to this thread and to another, delivered only in
 * alertable waits (SleepEx, WaitForSingleObjectEx), and alertable waits that
 * return WAIT_IO_COMPLETION.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static volatile LONG ran, order[8], norder;
static void CALLBACK apc(ULONG_PTR arg) { InterlockedIncrement(&ran); order[InterlockedIncrement(&norder) - 1] = (LONG)arg; }

static HANDLE never, started;
static volatile DWORD waiter_result;
static DWORD WINAPI waiter(void *p) {
    /* An APC queued before the thread has started runs during its startup
     * instead, so the wait must be reached first. */
    SetEvent(started);
    waiter_result = WaitForSingleObjectEx(never, 5000, TRUE);
    return 0;
}

int main(void) {
    QueueUserAPC(apc, GetCurrentThread(), 1);
    QueueUserAPC(apc, GetCurrentThread(), 2);
    CHECK("not delivered without an alertable wait", ran == 0 && (Sleep(1), ran == 0));
    DWORD r = SleepEx(1000, TRUE);
    CHECK("SleepEx returns WAIT_IO_COMPLETION", r == WAIT_IO_COMPLETION);
    CHECK("both APCs ran, in order", ran == 2 && order[0] == 1 && order[1] == 2);
    CHECK("an alertable wait with nothing queued times out", SleepEx(10, TRUE) == 0);

    never = CreateEventW(0, TRUE, FALSE, 0);
    started = CreateEventW(0, TRUE, FALSE, 0);
    HANDLE th = CreateThread(0, 0, waiter, 0, 0, 0);
    WaitForSingleObject(started, INFINITE);
    Sleep(30);
    QueueUserAPC(apc, th, 3);
    WaitForSingleObject(th, INFINITE);
    CHECK("an APC wakes another thread's alertable wait", waiter_result == WAIT_IO_COMPLETION && ran == 3 && order[2] == 3);
    printf("%d failures\n", failures);
    return failures;
}
