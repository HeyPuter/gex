/* Mutual exclusion under real contention, as Noita leans on it: eight
 * threads through msvcp120's _Mtx_lock (std::mutex, over ConcRT's
 * critical_section) when the DLL is beside the program, a CRITICAL_SECTION,
 * an SRW lock, and the interlocked SList (cmpxchg8b on i386). Each lock
 * guards a counter and a ring whose every slot holds a value and its
 * complement, so a section two threads were in at once shows as a torn slot
 * or a lost increment. Prints one line per check.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

#define THREADS 8
#define ITERS 5000

static volatile LONG counter, torn;
static volatile ULONG ring[64][2];
static volatile LONG head;

/* Inside the lock: bump the counter non-atomically and rewrite a ring slot
 * in two halves, checking the one before. */
static void critical(void) {
    LONG c = counter;
    LONG h = head;
    ULONG v = ring[h & 63][0], nv = ring[h & 63][1];
    if (v != ~nv) torn++;
    ring[(h + 1) & 63][0] = (ULONG)c;
    YieldProcessor();
    ring[(h + 1) & 63][1] = ~(ULONG)c;
    head = h + 1;
    counter = c + 1;
}

static void reset(void) {
    counter = torn = head = 0;
    for (int i = 0; i < 64; i++) { ring[i][0] = 0; ring[i][1] = ~0u; }
}

typedef void *mtx_t;
static int (__cdecl *mtx_init)(mtx_t *, int);
static int (__cdecl *mtx_lock)(mtx_t);
static int (__cdecl *mtx_unlock)(mtx_t);
static mtx_t mtx;
static DWORD WINAPI mtx_worker(void *p) {
    for (int i = 0; i < ITERS; i++) { mtx_lock(mtx); critical(); mtx_unlock(mtx); }
    return 0;
}

static CRITICAL_SECTION cs;
static DWORD WINAPI cs_worker(void *p) {
    for (int i = 0; i < ITERS; i++) { EnterCriticalSection(&cs); critical(); LeaveCriticalSection(&cs); }
    return 0;
}

static SRWLOCK srw = SRWLOCK_INIT;
static DWORD WINAPI srw_worker(void *p) {
    for (int i = 0; i < ITERS; i++) { AcquireSRWLockExclusive(&srw); critical(); ReleaseSRWLockExclusive(&srw); }
    return 0;
}

#define NODES 256
typedef struct { SLIST_ENTRY e; LONG owner; } node_t;
static SLIST_HEADER list;
static node_t *nodes;
static volatile LONG doubled;
static DWORD WINAPI slist_worker(void *p) {
    LONG me = (LONG)(INT_PTR)p + 1;
    for (int i = 0; i < ITERS; i++) {
        node_t *n = (node_t *)InterlockedPopEntrySList(&list);
        if (!n) continue;
        /* Popped twice at once would make two owners. */
        if (InterlockedExchange(&n->owner, me) != 0) doubled++;
        YieldProcessor();
        InterlockedExchange(&n->owner, 0);
        InterlockedPushEntrySList(&list, &n->e);
    }
    return 0;
}

static void run(LPTHREAD_START_ROUTINE f) {
    HANDLE t[THREADS];
    for (int i = 0; i < THREADS; i++) t[i] = CreateThread(0, 0, f, (void *)(INT_PTR)i, 0, 0);
    WaitForMultipleObjects(THREADS, t, TRUE, INFINITE);
    for (int i = 0; i < THREADS; i++) CloseHandle(t[i]);
}

static void report(const char *what) {
    char name[96];
    snprintf(name, sizeof name, "%s: %ld of %d, %ld torn", what, counter, THREADS * ITERS, torn);
    CHECK(name, counter == THREADS * ITERS && torn == 0);
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    HMODULE p = LoadLibraryA("msvcp120.dll");
    if (p) {
        mtx_init = (void *)GetProcAddress(p, "_Mtx_init");
        mtx_lock = (void *)GetProcAddress(p, "_Mtx_lock");
        mtx_unlock = (void *)GetProcAddress(p, "_Mtx_unlock");
        reset();
        mtx_init(&mtx, 1 /* _Mtx_plain */);
        run(mtx_worker);
        report("msvcp120 _Mtx_lock");
    }

    reset();
    InitializeCriticalSection(&cs);
    run(cs_worker);
    report("critical section");

    reset();
    run(srw_worker);
    report("SRW lock");

    InitializeSListHead(&list);
    nodes = _aligned_malloc(sizeof(node_t) * NODES, MEMORY_ALLOCATION_ALIGNMENT);
    for (int i = 0; i < NODES; i++) { nodes[i].owner = 0; InterlockedPushEntrySList(&list, &nodes[i].e); }
    run(slist_worker);
    int n = 0;
    while (InterlockedPopEntrySList(&list)) n++;
    char name[96];
    snprintf(name, sizeof name, "SList: %d of %d nodes back, %ld popped twice", n, NODES, doubled);
    CHECK(name, n == NODES && doubled == 0);

    printf("%d failures\n", failures);
    return failures;
}
