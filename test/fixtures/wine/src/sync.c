/* Events, mutexes (recursion, ownership, abandonment), semaphores, waits on
 * several objects, timeouts, and the futex-based primitives Wine builds on
 * NtWaitForAlertByThreadId: critical sections, SRW locks, condition
 * variables, WaitOnAddress, one-time init. Prints one line per check.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -lsynchronization */
#include <windows.h>
#include <stdio.h>

static int failures;
/* A wait that the test expects to succeed: it returns as soon as its object
 * is signalled, so only a failure waits this long, and a thread that starts
 * late on a loaded machine still meets it. */
#define LONG_WAIT 60000
/* The bound that tells a timeout honoured in the wrong unit from a slow one. */
#define WAITED_AT_MOST 20000
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static HANDLE ev_go, ev_done, mtx, sem;
static volatile LONG shared;

static DWORD WINAPI set_later(void *p) { Sleep(20); SetEvent((HANDLE)p); return 7; }

static DWORD WINAPI try_mutex(void *p) {
    DWORD r = WaitForSingleObject(mtx, 0);
    if (r == WAIT_OBJECT_0) ReleaseMutex(mtx);
    return r;
}

static DWORD WINAPI hold_and_die(void *p) { WaitForSingleObject(mtx, INFINITE); return 0; }

static DWORD WINAPI sem_taker(void *p) {
    for (int i = 0; i < 5; i++) WaitForSingleObject(sem, INFINITE);
    return 0;
}

static CRITICAL_SECTION cs;
static long long cs_counter;
static DWORD WINAPI cs_worker(void *p) {
    for (int i = 0; i < 20000; i++) { EnterCriticalSection(&cs); cs_counter++; LeaveCriticalSection(&cs); }
    return 0;
}

static SRWLOCK srw = SRWLOCK_INIT;
static CONDITION_VARIABLE cv = CONDITION_VARIABLE_INIT;
static int queue[64], qhead, qtail, produced_sum, consumed_sum;
static DWORD WINAPI producer(void *p) {
    for (int i = 1; i <= 200; i++) {
        AcquireSRWLockExclusive(&srw);
        while (qtail - qhead == 64) SleepConditionVariableSRW(&cv, &srw, INFINITE, 0);
        queue[qtail++ % 64] = i; produced_sum += i;
        ReleaseSRWLockExclusive(&srw);
        WakeAllConditionVariable(&cv);
    }
    return 0;
}
static DWORD WINAPI consumer(void *p) {
    for (int n = 0; n < 200; n++) {
        AcquireSRWLockExclusive(&srw);
        while (qtail == qhead) SleepConditionVariableSRW(&cv, &srw, INFINITE, 0);
        consumed_sum += queue[qhead++ % 64];
        ReleaseSRWLockExclusive(&srw);
        WakeAllConditionVariable(&cv);
    }
    return 0;
}

static volatile LONG addr_word;
static DWORD WINAPI wake_addr(void *p) { Sleep(20); InterlockedExchange(&addr_word, 1); WakeByAddressSingle((void *)&addr_word); return 0; }

static INIT_ONCE once = INIT_ONCE_STATIC_INIT;
static LONG once_runs;
static BOOL CALLBACK once_fn(INIT_ONCE *o, void *p, void **ctx) { InterlockedIncrement(&once_runs); Sleep(5); return TRUE; }
static DWORD WINAPI once_worker(void *p) { InitOnceExecuteOnce(&once, once_fn, 0, 0); return 0; }

static HANDLE start(LPTHREAD_START_ROUTINE f, void *p) { return CreateThread(0, 0, f, p, 0, 0); }

int main(void) {
    /* Events */
    HANDLE auto_ev = CreateEventW(0, FALSE, TRUE, 0);
    CHECK("auto event starts set", WaitForSingleObject(auto_ev, 0) == WAIT_OBJECT_0);
    CHECK("auto event resets", WaitForSingleObject(auto_ev, 0) == WAIT_TIMEOUT);
    HANDLE man = CreateEventW(0, TRUE, FALSE, 0);
    SetEvent(man);
    CHECK("manual event stays set", WaitForSingleObject(man, 0) == WAIT_OBJECT_0 && WaitForSingleObject(man, 0) == WAIT_OBJECT_0);
    ResetEvent(man);
    CHECK("manual event resets", WaitForSingleObject(man, 0) == WAIT_TIMEOUT);
    ULONGLONG t0 = GetTickCount64();
    CHECK("timeout", WaitForSingleObject(man, 50) == WAIT_TIMEOUT);
    ULONGLONG waited = GetTickCount64() - t0;
    CHECK("timeout lasts", waited >= 40 && waited < WAITED_AT_MOST);
    HANDLE th = start(set_later, man);
    CHECK("wait woken by another thread", WaitForSingleObject(man, LONG_WAIT) == WAIT_OBJECT_0);
    DWORD code = 0;
    CHECK("thread handle signalled", WaitForSingleObject(th, LONG_WAIT) == WAIT_OBJECT_0 && GetExitCodeThread(th, &code) && code == 7);
    CloseHandle(th);

    /* Named objects */
    HANDLE n1 = CreateEventW(0, TRUE, FALSE, L"gex-test-event");
    HANDLE n2 = CreateEventW(0, TRUE, FALSE, L"gex-test-event");
    CHECK("named event opens existing", n2 && GetLastError() == ERROR_ALREADY_EXISTS);
    SetEvent(n1);
    CHECK("named event is one object", WaitForSingleObject(n2, 0) == WAIT_OBJECT_0);
    HANDLE n3 = OpenEventW(EVENT_ALL_ACCESS, FALSE, L"gex-test-event");
    CHECK("OpenEvent", n3 != 0);

    /* Mutexes */
    mtx = CreateMutexW(0, TRUE, 0);
    CHECK("mutex recursive", WaitForSingleObject(mtx, 0) == WAIT_OBJECT_0);
    th = start(try_mutex, 0);
    WaitForSingleObject(th, INFINITE); GetExitCodeThread(th, &code); CloseHandle(th);
    CHECK("mutex excludes other threads", code == WAIT_TIMEOUT);
    CHECK("mutex release twice", ReleaseMutex(mtx) && ReleaseMutex(mtx));
    CHECK("mutex release unowned fails", !ReleaseMutex(mtx) && GetLastError() == ERROR_NOT_OWNER);
    th = start(try_mutex, 0);
    WaitForSingleObject(th, INFINITE); GetExitCodeThread(th, &code); CloseHandle(th);
    CHECK("mutex free for others", code == WAIT_OBJECT_0);
    th = start(hold_and_die, 0);
    WaitForSingleObject(th, INFINITE); CloseHandle(th);
    CHECK("abandoned mutex", WaitForSingleObject(mtx, 1000) == WAIT_ABANDONED);
    ReleaseMutex(mtx);

    /* Semaphores */
    sem = CreateSemaphoreW(0, 2, 5, 0);
    LONG prev = -1;
    CHECK("semaphore counts", WaitForSingleObject(sem, 0) == WAIT_OBJECT_0 && WaitForSingleObject(sem, 0) == WAIT_OBJECT_0 && WaitForSingleObject(sem, 0) == WAIT_TIMEOUT);
    CHECK("semaphore release", ReleaseSemaphore(sem, 3, &prev) && prev == 0);
    CHECK("semaphore limit", !ReleaseSemaphore(sem, 3, &prev) && GetLastError() == ERROR_TOO_MANY_POSTS);
    th = start(sem_taker, 0);
    Sleep(20);
    ReleaseSemaphore(sem, 2, 0);
    CHECK("semaphore wakes a waiter", WaitForSingleObject(th, LONG_WAIT) == WAIT_OBJECT_0);
    CloseHandle(th);

    /* Several objects */
    HANDLE a = CreateEventW(0, TRUE, FALSE, 0), b = CreateEventW(0, TRUE, FALSE, 0);
    HANDLE ab[2] = {a, b};
    SetEvent(b);
    CHECK("wait any", WaitForMultipleObjects(2, ab, FALSE, 0) == WAIT_OBJECT_0 + 1);
    CHECK("wait all not yet", WaitForMultipleObjects(2, ab, TRUE, 10) == WAIT_TIMEOUT);
    th = start(set_later, a);
    CHECK("wait all", WaitForMultipleObjects(2, ab, TRUE, LONG_WAIT) == WAIT_OBJECT_0);
    WaitForSingleObject(th, INFINITE); CloseHandle(th);
    HANDLE ae = CreateEventW(0, FALSE, TRUE, 0), be = CreateEventW(0, FALSE, FALSE, 0);
    HANDLE ab2[2] = {ae, be};
    CHECK("wait all consumes nothing when unsatisfied", WaitForMultipleObjects(2, ab2, TRUE, 0) == WAIT_TIMEOUT && WaitForSingleObject(ae, 0) == WAIT_OBJECT_0);
    CHECK("SignalObjectAndWait", SignalObjectAndWait(be, ae, 10, FALSE) == WAIT_TIMEOUT && WaitForSingleObject(be, 0) == WAIT_OBJECT_0);

    /* Futex-based primitives */
    InitializeCriticalSection(&cs);
    HANDLE ts[4];
    for (int i = 0; i < 4; i++) ts[i] = start(cs_worker, 0);
    WaitForMultipleObjects(4, ts, TRUE, INFINITE);
    for (int i = 0; i < 4; i++) CloseHandle(ts[i]);
    CHECK("critical section", cs_counter == 80000);
    ts[0] = start(producer, 0); ts[1] = start(consumer, 0);
    WaitForMultipleObjects(2, ts, TRUE, INFINITE);
    CHECK("SRW lock and condition variable", produced_sum == 20100 && consumed_sum == 20100);
    CloseHandle(ts[0]); CloseHandle(ts[1]);
    LONG zero = 0;
    th = start(wake_addr, 0);
    while (addr_word == 0) WaitOnAddress(&addr_word, &zero, sizeof(zero), INFINITE);
    CHECK("WaitOnAddress", addr_word == 1);
    WaitForSingleObject(th, INFINITE); CloseHandle(th);
    for (int i = 0; i < 4; i++) ts[i] = start(once_worker, 0);
    WaitForMultipleObjects(4, ts, TRUE, INFINITE);
    CHECK("InitOnceExecuteOnce", once_runs == 1);

    /* Waitable timers */
    HANDLE wt = CreateWaitableTimerW(0, FALSE, 0);
    LARGE_INTEGER due = {.QuadPart = -500000}; /* 50 ms */
    t0 = GetTickCount64();
    CHECK("SetWaitableTimer", SetWaitableTimer(wt, &due, 0, 0, 0, FALSE));
    /* Not signalled before it is due; a stall past the due time makes the
     * check say nothing. */
    CHECK("timer not yet", WaitForSingleObject(wt, 0) == WAIT_TIMEOUT || GetTickCount64() - t0 >= 50);
    CHECK("timer fires", WaitForSingleObject(wt, LONG_WAIT) == WAIT_OBJECT_0 && GetTickCount64() - t0 >= 40);
    CHECK("synchronization timer resets", WaitForSingleObject(wt, 0) == WAIT_TIMEOUT);
    due.QuadPart = -100000;
    SetWaitableTimer(wt, &due, 10, 0, 0, FALSE);
    int fired = 0;
    for (int i = 0; i < 3; i++) fired += WaitForSingleObject(wt, LONG_WAIT) == WAIT_OBJECT_0;
    CHECK("periodic timer", fired == 3);
    /* Cancelling stops further periods but, as on Windows, leaves a period
     * that already elapsed signalled (it may land between the last wait and
     * the cancel under load): consume at most that one, then nothing more
     * may fire. */
    BOOL cancelled = CancelWaitableTimer(wt);
    WaitForSingleObject(wt, 0);
    CHECK("CancelWaitableTimer", cancelled && WaitForSingleObject(wt, 50) == WAIT_TIMEOUT);
    CloseHandle(wt);

    /* Sleep and clocks */
    LARGE_INTEGER f, q0, q1;
    QueryPerformanceFrequency(&f); QueryPerformanceCounter(&q0);
    Sleep(30);
    QueryPerformanceCounter(&q1);
    double ms = (double)(q1.QuadPart - q0.QuadPart) * 1000.0 / f.QuadPart;
    CHECK("Sleep", ms >= 25 && ms < WAITED_AT_MOST);

    printf("%d failures\n", failures);
    return failures;
}
