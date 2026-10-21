#include <stdio.h>
#include <windows.h>
static DWORD tls, fls;
static CRITICAL_SECTION lock;
static volatile unsigned progress, cleaned;
static HANDLE event;
static HANDLE (WINAPI *current_thread)(void);
static DWORD message_target;
static DWORD WINAPI post_thread(void *p) {
    return PostThreadMessageW(message_target, WM_APP + 7, 123, 456) ? 42 : 1;
}
static DWORD WINAPI signal_event(void *p) {
    Sleep(10);
    return SetEvent(event) ? 42 : 1;
}
static DWORD WINAPI recycled_worker(void *p) {
    if (TlsGetValue(tls) || FlsGetValue(fls) || GetLastError()) {
        return 10;
    }
    MSG message;
    if (PeekMessageW(&message, 0, 0, 0, PM_REMOVE)) {
        return 11;
    }
    if (!TlsSetValue(tls, p) || !FlsSetValue(fls, (void *)(ULONG_PTR)GetCurrentThreadId())) {
        return 12;
    }
    if (!PostThreadMessageW(GetCurrentThreadId(), WM_APP + 8, 0, 0) || !SetTimer(0, 0, 10, 0)) {
        return 13;
    }
    SetLastError(99);
    return 42;
}
static void WINAPI cleanup(void *p) {
    if ((ULONG_PTR)p == GetCurrentThreadId()) {
        ++cleaned;
    }
}
static DWORD WINAPI worker(void *p) {
    if (TlsGetValue(tls) || FlsGetValue(fls) || GetLastError()) {
        return 10;
    }
    DWORD id = GetCurrentThreadId();
    TlsSetValue(tls, p);
    FlsSetValue(fls, (void *)(ULONG_PTR)id);
    SetLastError(id + 100);
    Sleep(2);
    if (GetCurrentThread() != (HANDLE)-2 || current_thread() != (HANDLE)-2 || GetLastError() != id + 100) {
        return 39;
    }
    if (GetLastError() != id + 100 || TlsGetValue(tls) != p) {
        return 11;
    }
    EnterCriticalSection(&lock);
    ++progress;
    LeaveCriticalSection(&lock);
    static unsigned char bytes[51600];
    for (unsigned i = 0; i < sizeof(bytes); ++i) {
        bytes[i] = i;
    }
    /* Host libc must run on a16-byte-aligned C stack. */
    if (puts("worker") < 0) {
        return 12;
    }
    if ((ULONG_PTR)p == 2) {
        ExitThread(42);
    }
    return 42;
}
#define HANDLE_CHECK(test) do { if (!(test)) { printf("FAIL thread handles line %d\n", __LINE__); return 1; } } while (0)
static struct {
    HANDLE main, self, ready, finish;
} aliases;
static unsigned probe_runs;
static DWORD WINAPI handle_probe(void *p) {
    ++probe_runs;
    return 42;
}
static DWORD WINAPI alias_worker(void *p) {
    DWORD code;
    HANDLE process = GetCurrentProcess();
    HANDLE_CHECK(GetExitCodeThread(aliases.main, &code) && code == STILL_ACTIVE);
    HANDLE_CHECK(WaitForSingleObject(aliases.main, 0) == WAIT_TIMEOUT);
    HANDLE_CHECK(DuplicateHandle(process, GetCurrentThread(), process, &aliases.self, 0, FALSE, DUPLICATE_SAME_ACCESS));
    HANDLE_CHECK(GetThreadPriority(aliases.self) == THREAD_PRIORITY_NORMAL);
    HANDLE_CHECK(SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL));
    HANDLE_CHECK(CloseHandle(GetCurrentThread()));
    HANDLE_CHECK(WaitForSingleObject(GetCurrentThread(), 1) == WAIT_TIMEOUT);
    HANDLE_CHECK(SetEvent(aliases.ready) && WaitForSingleObject(aliases.finish, 2000) == WAIT_OBJECT_0);
    return 42;
}
static int check_thread_handles(void) {
    HANDLE process = GetCurrentProcess();
    BOOL (WINAPI *duplicate)(HANDLE, HANDLE, HANDLE, LPHANDLE, DWORD, BOOL, DWORD) =
        (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "DuplicateHandle");
    int (WINAPI *priority)(HANDLE) = (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "GetThreadPriority");
    BOOL (WINAPI *set_priority)(HANDLE, int) = (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "SetThreadPriority");
    HANDLE_CHECK(duplicate && priority && set_priority);
    struct { DWORD before; HANDLE handle; DWORD after; } output = { 0x12345678, 0, 0x87654321 };
    DWORD code;
    SetLastError(77);
    HANDLE_CHECK(DuplicateHandle(process, GetCurrentThread(), process, &output.handle, 0xdeadbeef, FALSE, DUPLICATE_SAME_ACCESS));
    HANDLE_CHECK(output.handle && output.handle != GetCurrentThread() && output.before == 0x12345678 && output.after == 0x87654321);
    HANDLE_CHECK(GetLastError() == 77 && GetExitCodeThread(output.handle, &code) && code == STILL_ACTIVE);
    HANDLE_CHECK(duplicate(process, output.handle, process, &aliases.main, 0, FALSE, DUPLICATE_SAME_ACCESS));
    HANDLE_CHECK(aliases.main != output.handle && CloseHandle(output.handle));
    HANDLE_CHECK(!CloseHandle(output.handle) && GetLastError() == ERROR_INVALID_HANDLE);
    HANDLE_CHECK(GetThreadPriority(output.handle) == THREAD_PRIORITY_ERROR_RETURN && GetLastError() == ERROR_INVALID_HANDLE);
    SetLastError(77);
    HANDLE_CHECK(priority(aliases.main) == THREAD_PRIORITY_NORMAL && set_priority(aliases.main, THREAD_PRIORITY_NORMAL));
    HANDLE_CHECK(CloseHandle(GetCurrentThread()) && CloseHandle(process) && GetLastError() == 77);
    HANDLE_CHECK(GetExitCodeThread(GetCurrentThread(), &code) && code == STILL_ACTIVE && ResumeThread(GetCurrentThread()) == 0);
    HANDLE_CHECK(WaitForSingleObject(GetCurrentThread(), 0) == WAIT_TIMEOUT && WaitForSingleObject(aliases.main, 1) == WAIT_TIMEOUT);
    HANDLE_CHECK(!set_priority(aliases.main, THREAD_PRIORITY_TIME_CRITICAL) && GetLastError() == ERROR_NOT_SUPPORTED);
    HANDLE_CHECK(priority(aliases.main) == THREAD_PRIORITY_NORMAL);
    HANDLE_CHECK(!SetThreadPriority(0, 0) && GetLastError() == ERROR_INVALID_HANDLE);

    aliases.ready = CreateEventW(0, FALSE, FALSE, 0);
    aliases.finish = CreateEventW(0, FALSE, FALSE, 0);
    HANDLE worker = CreateThread(0, 0, alias_worker, 0, CREATE_SUSPENDED, 0), other;
    HANDLE_CHECK(aliases.ready && aliases.finish && worker);
    HANDLE_CHECK(duplicate(process, worker, process, &other, 0, FALSE, DUPLICATE_SAME_ACCESS));
    HANDLE_CHECK(CloseHandle(worker) && ResumeThread(worker) == (DWORD)-1 && GetLastError() == ERROR_INVALID_HANDLE);
    HANDLE_CHECK(ResumeThread(other) == 1 && WaitForSingleObject(aliases.ready, 2000) == WAIT_OBJECT_0);
    HANDLE_CHECK(aliases.self != other && GetExitCodeThread(aliases.self, &code) && code == STILL_ACTIVE);
    HANDLE_CHECK(CloseHandle(other) && SetEvent(aliases.finish) && WaitForSingleObject(aliases.self, 2000) == WAIT_OBJECT_0);
    Sleep(0);
    HANDLE_CHECK(GetExitCodeThread(aliases.self, &code) && code == 42);
    HANDLE_CHECK(GetExitCodeThread(aliases.main, &code) && code == STILL_ACTIVE);
    HANDLE_CHECK(duplicate(process, aliases.self, process, &other, 0, FALSE, DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE));
    HANDLE_CHECK(!GetExitCodeThread(aliases.self, &code) && GetLastError() == ERROR_INVALID_HANDLE);
    HANDLE_CHECK(GetExitCodeThread(other, &code) && code == 42 && priority(other) == THREAD_PRIORITY_NORMAL);
    SetLastError(77);
    HANDLE_CHECK(duplicate(process, other, 0, (LPHANDLE)0xfffffffeu, ~0u, TRUE, DUPLICATE_CLOSE_SOURCE) && GetLastError() == 77);
    HANDLE_CHECK(!GetExitCodeThread(other, &code) && GetLastError() == ERROR_INVALID_HANDLE);
    HANDLE_CHECK(CloseHandle(aliases.ready) && CloseHandle(aliases.finish));

    HANDLE unchanged = (HANDLE)0x12345678;
    HANDLE_CHECK(!duplicate((HANDLE)1, aliases.main, process, &unchanged, 0, FALSE, 3) && GetLastError() == ERROR_INVALID_HANDLE);
    HANDLE_CHECK(GetExitCodeThread(aliases.main, &code) && code == STILL_ACTIVE);
    HANDLE_CHECK(!duplicate(process, 0, process, &unchanged, 0, FALSE, 2) && GetLastError() == ERROR_INVALID_HANDLE);
    HANDLE_CHECK(!duplicate(process, other, process, &unchanged, 0, FALSE, 2) && GetLastError() == ERROR_INVALID_HANDLE);
    HANDLE_CHECK(!duplicate(process, aliases.main, process, &unchanged, 0, TRUE, 2) && GetLastError() == ERROR_NOT_SUPPORTED);
    HANDLE_CHECK(!duplicate(process, aliases.main, process, &unchanged, 0, FALSE, 0) && GetLastError() == ERROR_NOT_SUPPORTED);
    for (unsigned mode = 0; mode < 3; ++mode) {
        HANDLE_CHECK(duplicate(process, aliases.main, process, &other, 0, FALSE, 2));
        HANDLE_CHECK(!duplicate(process, other, mode == 0 ? (HANDLE)1 : process, &unchanged, 0, mode == 1,
                                mode == 2 ? 0x83 : 3));
        HANDLE_CHECK(GetLastError() == (mode == 0 ? ERROR_INVALID_HANDLE : mode == 1 ? ERROR_NOT_SUPPORTED : ERROR_INVALID_PARAMETER));
        HANDLE_CHECK(!GetExitCodeThread(other, &code) && GetLastError() == ERROR_INVALID_HANDLE);
    }
    HANDLE event = CreateEventW(0, FALSE, TRUE, 0);
    HANDLE_CHECK(event && !duplicate(process, event, process, &unchanged, 0, FALSE, 2) && GetLastError() == ERROR_NOT_SUPPORTED);
    HANDLE_CHECK(WaitForSingleObject(event, 0) == WAIT_OBJECT_0);
    HANDLE_CHECK(!duplicate(process, event, process, &unchanged, 0, FALSE, 3) && GetLastError() == ERROR_NOT_SUPPORTED);
    HANDLE_CHECK(!SetEvent(event) && GetLastError() == ERROR_INVALID_HANDLE);
    HANDLE_CHECK(unchanged == (HANDLE)0x12345678);

    HANDLE handles[128];
    unsigned count;
    for (count = 0; count < 128; ++count) {
        handles[count] = (HANDLE)0x12345678;
        if (!duplicate(process, aliases.main, process, &handles[count], 0, FALSE, 2)) {
            break;
        }
    }
    HANDLE_CHECK(count == 127 && GetLastError() == ERROR_NOT_ENOUGH_MEMORY && handles[count] == (HANDLE)0x12345678);
    DWORD id = 0x12345678;
    HANDLE_CHECK(!CreateThread(0, 0, handle_probe, 0, 0, &id) && GetLastError() == ERROR_NOT_ENOUGH_MEMORY && id == 0x12345678);
    Sleep(0);
    HANDLE_CHECK(!probe_runs);
    HANDLE_CHECK(!duplicate(process, handles[0], process, &unchanged, 0, FALSE, 3) && GetLastError() == ERROR_NOT_ENOUGH_MEMORY);
    HANDLE_CHECK(!GetExitCodeThread(handles[0], &code) && GetLastError() == ERROR_INVALID_HANDLE);
    worker = CreateThread(0, 0, handle_probe, 0, 0, 0);
    HANDLE_CHECK(worker && WaitForSingleObject(worker, 2000) == WAIT_OBJECT_0 && CloseHandle(worker));
    HANDLE_CHECK(probe_runs == 1 && unchanged == (HANDLE)0x12345678);
    for (unsigned i = 1; i < count; ++i) {
        HANDLE_CHECK(CloseHandle(handles[i]));
    }

    /* Exhaust guest memory, then retry creation beyond the handle-table capacity. */
    void *blocks[128];
    unsigned blocks_used;
    for (blocks_used = 0; blocks_used < 128; ++blocks_used) {
        blocks[blocks_used] = HeapAlloc(GetProcessHeap(), 0, 1024 * 1024);
        if (!blocks[blocks_used]) {
            break;
        }
    }
    HANDLE_CHECK(blocks_used < 128);
    for (unsigned i = 0; i < 140; ++i) {
        HANDLE_CHECK(!CreateThread(0, 8 * 1024 * 1024, handle_probe, 0, 0, &id) && GetLastError() == ERROR_NOT_ENOUGH_MEMORY);
        HANDLE_CHECK(id == 0x12345678);
    }
    for (unsigned i = 0; i < blocks_used; ++i) {
        HANDLE_CHECK(HeapFree(GetProcessHeap(), 0, blocks[i]));
    }
    worker = CreateThread(0, 0, handle_probe, 0, 0, 0);
    HANDLE_CHECK(worker && WaitForSingleObject(worker, 2000) == WAIT_OBJECT_0 && CloseHandle(worker) && probe_runs == 2);
    HANDLE_CHECK(CloseHandle(aliases.main));
    SetLastError(77);
    HANDLE_CHECK(duplicate(process, GetCurrentThread(), process, 0, 0, FALSE, 3) && GetLastError() == 77);
    HANDLE_CHECK(GetExitCodeThread(GetCurrentThread(), &code) && code == STILL_ACTIVE);
    puts("PASS thread handles32");
    return 0;
}
#undef HANDLE_CHECK
int entry(void) {
    current_thread = (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "GetCurrentThread");
    if (!current_thread) {
        return 40;
    }
    SetLastError(77);
    if (GetCurrentThread() != (HANDLE)-2 || current_thread() != (HANDLE)-2 || GetCurrentProcess() != (HANDLE)-1 ||
        GetLastError() != 77) {
        return 41;
    }
    if (check_thread_handles()) {
        return 42;
    }
    tls = TlsAlloc();
    fls = FlsAlloc(cleanup);
    TlsSetValue(tls, (void *)99);
    SetLastError(77);
    InitializeCriticalSection(&lock);
    EnterCriticalSection(&lock);
    HANDLE a = CreateThread(0, 0, worker, (void *)1, CREATE_SUSPENDED, 0), b = CreateThread(0, 0, worker, (void *)2, 0, 0);
    if (!a || !b || ResumeThread(a) != 1) {
        return 1;
    }
    SetLastError(77);
    Sleep(10);
    if (progress || GetLastError() != 77 || TlsGetValue(tls) != (void *)99) {
        return 2;
    }
    LeaveCriticalSection(&lock);
    if (WaitForSingleObject(a, 2000) || WaitForSingleObject(b, 2000)) {
        return 3;
    }
    DWORD code;
    if (!GetExitCodeThread(a, &code) || code != 42 || !GetExitCodeThread(b, &code) || code != 42) {
        return 4;
    }
    if (progress != 2 || cleaned != 2 || !CloseHandle(a) || !CloseHandle(b) || GetExitCodeThread(a, &code)) {
        return 5;
    }
    event = CreateEventA(0, FALSE, FALSE, 0);
    if (!event || WaitForSingleObject(event, 0) != WAIT_TIMEOUT) {
        return 6;
    }
    if (MsgWaitForMultipleObjects(1, &event, FALSE, 0, QS_ALLINPUT) != WAIT_TIMEOUT) {
        return 20;
    }
    if (!SetEvent(event) || MsgWaitForMultipleObjects(1, &event, FALSE, 0, QS_ALLINPUT) != 0 ||
        WaitForSingleObject(event, 0) != WAIT_TIMEOUT) {
        return 21;
    }
    UINT_PTR timer = SetTimer(0, 0, 10, 0);
    if (!timer || MsgWaitForMultipleObjects(1, &event, FALSE, 2000, QS_TIMER) != 1) {
        return 22;
    }
    MSG message;
    if (!PeekMessageW(&message, 0, WM_TIMER, WM_TIMER, PM_REMOVE) || message.wParam != timer || !KillTimer(0, timer)) {
        return 23;
    }
    message_target = GetCurrentThreadId();
    if (PostThreadMessageW(0, WM_APP, 0, 0) || GetLastError() != ERROR_INVALID_THREAD_ID) {
        return 24;
    }
    DWORD sender_id;
    HANDLE sender = CreateThread(0, 0, post_thread, 0, CREATE_SUSPENDED, &sender_id);
    if (!sender || PostThreadMessageW(sender_id, WM_APP, 0, 0) || GetLastError() != ERROR_INVALID_THREAD_ID) {
        return 25;
    }
    if (ResumeThread(sender) != 1 || WaitForSingleObject(sender, 2000) || !GetExitCodeThread(sender, &code) || code != 42) {
        return 26;
    }
    if (!PeekMessageW(&message, (HWND)-1, WM_APP + 7, WM_APP + 7, PM_REMOVE) || message.hwnd || message.wParam != 123 ||
        message.lParam != 456) {
        return 27;
    }
    if (PostThreadMessageW(sender_id, WM_APP, 0, 0) || GetLastError() != ERROR_INVALID_THREAD_ID || !CloseHandle(sender)) {
        return 28;
    }
    HANDLE c = CreateThread(0, 0, signal_event, 0, 0, 0);
    if (!c || WaitForSingleObject(event, 2000) || WaitForSingleObject(event, 0) != WAIT_TIMEOUT || WaitForSingleObject(c, 2000)) {
        return 7;
    }
    if (!CloseHandle(c) || !CloseHandle(event) || SetEvent(event) || GetLastError() != ERROR_INVALID_HANDLE) {
        return 8;
    }
    HANDLE (WINAPI *create_event_a)(LPSECURITY_ATTRIBUTES, BOOL, BOOL, LPCSTR) =
        (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "CreateEventA");
    SECURITY_ATTRIBUTES security = { sizeof(security), 0, FALSE };
    if (!create_event_a || create_event_a(&security, FALSE, FALSE, 0) || GetLastError() != ERROR_NOT_SUPPORTED) {
        return 38;
    }
    event = create_event_a(0, TRUE, TRUE, 0);
    if (!event || WaitForSingleObject(event, 0) || WaitForSingleObject(event, 0) || !ResetEvent(event) ||
        WaitForSingleObject(event, 0) != WAIT_TIMEOUT || !CloseHandle(event)) {
        return 9;
    }
    /* More total creations than runtime slots; retain one finished handle. */
    HANDLE retained = CreateThread(0, 0, recycled_worker, (void *)1, 0, 0);
    if (!retained || WaitForSingleObject(retained, 2000)) {
        return 29;
    }
    HANDLE stale = 0;
    for (unsigned i = 0; i < 130; ++i) {
        DWORD id;
        HANDLE thread = CreateThread(0, 0, recycled_worker, (void *)1, CREATE_SUSPENDED, &id);
        if (!thread) {
            return 30;
        }
        if (stale && (stale == thread || GetExitCodeThread(stale, &code) || GetLastError() != ERROR_INVALID_HANDLE)) {
            return 43;
        }
        if (PostThreadMessageW(id, WM_APP, 0, 0) || GetLastError() != ERROR_INVALID_THREAD_ID) {
            return 31;
        }
        if (ResumeThread(thread) != 1 || WaitForSingleObject(thread, 2000) || !GetExitCodeThread(thread, &code) || code != 42) {
            return 32;
        }
        if (!CloseHandle(thread) || GetExitCodeThread(thread, &code) || GetLastError() != ERROR_INVALID_HANDLE) {
            return 33;
        }
        if (!GetExitCodeThread(retained, &code) || code != 42) {
            return 34;
        }
        stale = thread;
    }
    if (!CloseHandle(retained) || cleaned != 133) {
        return 35;
    }
    /* Closing a running thread must not reclaim its stack or slot. */
    for (unsigned i = 0; i < 70; ++i) {
        event = CreateEventW(0, FALSE, FALSE, 0);
        HANDLE detached = CreateThread(0, 0, signal_event, 0, 0, 0);
        if (!event || !detached || !CloseHandle(detached)) {
            return 36;
        }
        HANDLE next = CreateThread(0, 0, recycled_worker, (void *)1, 0, 0);
        if (!next || WaitForSingleObject(next, 2000) || !GetExitCodeThread(next, &code) || code != 42 || !CloseHandle(next)) {
            return 37;
        }
        if (WaitForSingleObject(event, 2000) || !CloseHandle(event)) {
            return 38;
        }
    }
    FlsFree(fls);
    TlsFree(tls);
    DeleteCriticalSection(&lock);
    puts("PASS threads32");
    return 0;
}
