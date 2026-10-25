/* win32u's session reads against the server's writes (H, the session seqlock
 * cache): a writer thread bursts writes to one shared object while reader
 * threads loop reading it, on a window (its extra bytes), a queue (posted
 * messages: GetQueueStatus and an empty PeekMessage, the shared-queue fast
 * paths) and an input (the key state of two attached threads).
 *
 * The writer publishes each write's number once the write has returned, so
 * a read that starts after the publication must see that write or a later
 * one: an older value is a stale copy. Each value carries its number twice,
 * so a copy mixed from two states shows as torn. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

/* writes per case: argv[1], 20000 by default (the page runs fewer) */
static LONG64 WRITES = 20000;
#define READERS 2
#define MAX_READERS 8

static int failures;
static volatile LONG64 published;
static volatile LONG done;

static void check(int ok, const char *what, long long got)
{
    if (ok) printf("ok %s (%lld)\n", what, got);
    else { printf("FAIL %s (%lld)\n", what, got); failures++; }
}

/* window: a pointer-sized extra value, the write's number twice (64-bit:
 * once in each half) */
static HWND hwnd;
static LONG64 stale[MAX_READERS], torn[MAX_READERS], reads[MAX_READERS];
static LONG64 window_writes;

#ifdef _WIN64
static ULONG_PTR enc(LONG64 n) { return ((ULONG_PTR)(ULONG)n << 32) | ((ULONG)n ^ 0xa5a5a5a5u); }
static int enc_ok(ULONG_PTR v) { return (ULONG)(v >> 32) == ((ULONG)v ^ 0xa5a5a5a5u); }
static LONG64 dec(ULONG_PTR v) { return (LONG64)(v >> 32); }
#else
/* 32-bit (W3): the number in the top 20 bits (writes up to 2^20), a check
 * of it in the low 12 */
static ULONG_PTR enc(LONG64 n) { return ((ULONG)n << 12) | (((ULONG)n ^ 0xa5au) & 0xfffu); }
static int enc_ok(ULONG_PTR v) { return (((v >> 12) ^ 0xa5au) & 0xfffu) == (v & 0xfffu); }
static LONG64 dec(ULONG_PTR v) { return (LONG64)(v >> 12); }
#endif

static DWORD WINAPI window_writer(void *arg)
{
    LONG64 n;
    for (n = 1; n <= window_writes; n++)
    {
        SetWindowLongPtrW(hwnd, 0, enc(n));
        InterlockedExchange64(&published, n);
        if (!(n % 64)) Sleep(0);  /* bursts of 64 */
    }
    InterlockedExchange(&done, 1);
    return 0;
}

static DWORD WINAPI window_reader(void *arg)
{
    int r = (int)(ULONG_PTR)arg;
    while (!done)
    {
        LONG64 m = published, k;
        ULONG_PTR v = GetWindowLongPtrW(hwnd, 0);
        reads[r]++;
        if (!v && !m) continue;
        if (!enc_ok(v)) { if (torn[r]++ < 4) printf("torn %#llx\n", (unsigned long long)v); continue; }
        k = dec(v);
        if (k < m && stale[r]++ < 4) printf("window: read %lld after %lld was published\n", k, m);
    }
    return 0;
}

/* READERS threads reading the window at once: their copies of it overlap,
 * each into the one mirror of the session a process has */
static void window_case(int readers, LONG64 writes, const char *name)
{
    WNDCLASSW wc = {0};
    HANDLE t[MAX_READERS + 1];
    LONG64 s = 0, tr = 0, n = 0;
    char what[96];
    int i;

    wc.lpfnWndProc = DefWindowProcW;
    wc.cbWndExtra = 16;
    wc.lpszClassName = L"seqrace";
    RegisterClassW(&wc);
    hwnd = CreateWindowW(L"seqrace", L"", 0, 0, 0, 0, 0, HWND_MESSAGE, NULL, NULL, NULL);
    sprintf(what, "%s: created", name);
    check(hwnd != NULL, what, 0);
    if (!hwnd) return;
    published = 0; done = 0; window_writes = writes;
    memset(stale, 0, sizeof(stale)); memset(torn, 0, sizeof(torn)); memset(reads, 0, sizeof(reads));
    for (i = 0; i < readers; i++) t[i] = CreateThread(NULL, 0, window_reader, (void *)(ULONG_PTR)i, 0, NULL);
    t[readers] = CreateThread(NULL, 0, window_writer, NULL, 0, NULL);
    WaitForMultipleObjects(readers + 1, t, TRUE, INFINITE);
    for (i = 0; i <= readers; i++) CloseHandle(t[i]);
    for (i = 0; i < readers; i++) { s += stale[i]; tr += torn[i]; n += reads[i]; }
    sprintf(what, "%s: reads during the writes", name);
    check(n > writes / 4, what, n);
    sprintf(what, "%s: torn reads", name);
    check(!tr, what, tr);
    sprintf(what, "%s: stale reads", name);
    check(!s, what, s);
    sprintf(what, "%s: the last write read back", name);
    check(GetWindowLongPtrW(hwnd, 0) == enc(writes), what, writes);
    DestroyWindow(hwnd);
}

/* queue: the writer posts message i to the reader and waits for it to be
 * taken. `before` is raised before each post, `published` after it. */
static DWORD reader_tid;
static volatile LONG64 before, acked;
static volatile LONG give_up;
static LONG64 q_stale_missing, q_stale_present, q_peek_missing, q_wrong, q_reads;

static DWORD WINAPI queue_writer(void *arg)
{
    LONG64 i;
    for (i = 1; i <= WRITES; i++)
    {
        InterlockedExchange64(&before, i);
        PostThreadMessageW(reader_tid, WM_APP, (WPARAM)i, 0);
        InterlockedExchange64(&published, i);
        while (acked != i && !give_up) if (!(i % 64)) SwitchToThread(); else YieldProcessor();
        if (give_up) break;
    }
    InterlockedExchange(&done, 1);
    return 0;
}

static void queue_case(void)
{
    MSG msg;
    HANDLE w;
    LONG64 have = 0;

    PeekMessageW(&msg, NULL, 0, 0, PM_NOREMOVE);  /* the reader's queue */
    reader_tid = GetCurrentThreadId();
    published = before = acked = 0; done = 0;
    w = CreateThread(NULL, 0, queue_writer, NULL, 0, NULL);
    while (!done)
    {
        LONG64 p = published, q;
        DWORD s = HIWORD(GetQueueStatus(QS_POSTMESSAGE));
        BOOL peek = PeekMessageW(&msg, NULL, 0, 0, PM_NOREMOVE);
        q_reads++;
        if (p > have)
        {
            /* posted before these reads began: both must see it */
            if (!(s & QS_POSTMESSAGE) && q_stale_missing++ < 4) printf("queue: status without message %lld\n", p);
            if (!peek && q_peek_missing++ < 4) printf("queue: peek without message %lld\n", p);
            memset(&msg, 0, sizeof(msg));
            if (!PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE) || msg.message != WM_APP || (LONG64)msg.wParam != p)
            {
                if (q_wrong++ < 4) printf("queue: took %#x %lld, wanted %lld\n", msg.message, (LONG64)msg.wParam, p);
                break;
            }
            have = p;
            InterlockedExchange64(&acked, p);
            continue;
        }
        /* the reader took everything up to `have`: a message is seen only
         * once the next post has begun */
        q = before;
        if ((s & QS_POSTMESSAGE) && q <= have && q_stale_present++ < 4) printf("queue: status shows taken message %lld\n", have);
    }
    if (!done) InterlockedExchange(&give_up, 1);
    WaitForSingleObject(w, INFINITE);
    CloseHandle(w);
    check(q_reads > WRITES, "queue: reads during the posts", q_reads);
    check(!q_wrong, "queue: messages taken in order", q_wrong);
    check(!q_stale_missing, "queue: status missing a posted message", q_stale_missing);
    check(!q_peek_missing, "queue: empty peek after a post", q_peek_missing);
    check(!q_stale_present, "queue: status showing a taken message", q_stale_present);
}

/* input: two attached threads' key state, all 0x81 or all 0 by the write's parity */
static LONG64 k_stale, k_torn;
static volatile LONG64 k_reads;
static volatile DWORD writer_tid;
static volatile LONG attached;

static DWORD WINAPI key_writer(void *arg)
{
    BYTE st[256];
    LONG64 n;
    MSG msg;

    PeekMessageW(&msg, NULL, 0, 0, PM_NOREMOVE);
    writer_tid = GetCurrentThreadId();
    while (!attached) YieldProcessor();
    for (n = 1; n <= WRITES; n++)
    {
        memset(st, n & 1 ? 0x81 : 0, sizeof(st));
        InterlockedExchange64(&before, n);
        SetKeyboardState(st);
        InterlockedExchange64(&published, n);
        if (!(n % 64))
        {
            /* a burst at a time: the reader reads between bursts (a slow
             * read, a request each without the door, could miss them all) */
            LONG64 r = k_reads;
            DWORD t0 = GetTickCount();
            while (k_reads == r && GetTickCount() - t0 < 1000) Sleep(0);
        }
    }
    InterlockedExchange(&done, 1);
    return 0;
}

static void key_case(void)
{
    BYTE st[256];
    HANDLE w;
    int i;

    published = before = 0; done = 0; writer_tid = 0;
    w = CreateThread(NULL, 0, key_writer, NULL, 0, NULL);
    while (!writer_tid) YieldProcessor();
    check(AttachThreadInput(GetCurrentThreadId(), writer_tid, TRUE), "input: attached", 0);
    InterlockedExchange(&attached, 1);
    while (!done)
    {
        LONG64 m = published;
        BYTE want = m & 1 ? 0x81 : 0;
        GetKeyboardState(st);
        k_reads++;
        for (i = 1; i < 256; i++) if (st[i] != st[0]) break;
        if (i < 256) { if (k_torn++ < 4) printf("input: torn key state at %lld\n", m); continue; }
        /* a later write may already be in: stale only if none had begun since */
        if (m && st[0] != want && before == m && k_stale++ < 4) printf("input: state of %lld after %lld was published\n", m - 1, m);
    }
    WaitForSingleObject(w, INFINITE);
    CloseHandle(w);
    check(k_reads >= WRITES / 64, "input: reads during the writes", k_reads);
    check(!k_torn, "input: torn key states", k_torn);
    check(!k_stale, "input: stale key states", k_stale);
}

/* a read that never sees a write can stall a case: fail it instead */
static volatile const char *phase_name;
static HANDLE quit;
static int cases = 7;
static DWORD WINAPI watchdog(void *arg)
{
    const char *last = NULL;
    for (;;)
    {
        const char *now = (const char *)phase_name;
        if (!WaitForSingleObject(quit, 60000)) return 0;
        if (now == phase_name && now == last)
        {
            printf("FAIL %s: no progress for two minutes\n%d failures\n", now, failures + 1);
            ExitProcess(1);
        }
        last = now;
    }
}

int main(int argc, char **argv)
{
    HANDLE dog;

    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && atoi(argv[1]) > 0) WRITES = atoi(argv[1]);
    /* argv[2]: the cases to run, a mask (1 window, 2 queue, 4 input; 7 by
     * default; 8, 16 and 32 are cut-down window cases for the SP exit hang) */
    if (argc > 2) cases = atoi(argv[2]);
    quit = CreateEventW(NULL, TRUE, FALSE, NULL);
    dog = CreateThread(NULL, 0, watchdog, NULL, 0, NULL);
    if (cases & 1)
    {
        phase_name = "window";
        window_case(READERS, WRITES, "window");
        phase_name = "window, overlapping copies";
        window_case(MAX_READERS, 5 * WRITES, "window, 8 readers");
    }
    if (cases & 8)  /* (diagnosis: the window, read once, no threads) */
    {
        WNDCLASSW wc = {0};
        HWND h;
        wc.lpfnWndProc = DefWindowProcW;
        wc.cbWndExtra = 16;
        wc.lpszClassName = L"seqrace8";
        RegisterClassW(&wc);
        h = CreateWindowW(L"seqrace8", L"", 0, 0, 0, 0, 0, HWND_MESSAGE, NULL, NULL, NULL);
        SetWindowLongPtrW(h, 0, 5);
        check(GetWindowLongPtrW(h, 0) == 5, "window alone: read back", 5);
        DestroyWindow(h);
    }
    if (cases & 16)  /* (diagnosis: two readers, one writer) */
        window_case(READERS, WRITES, "window");
    if (cases & 32)  /* (diagnosis: eight readers) */
        window_case(MAX_READERS, WRITES, "window, 8 readers");
    if (cases & 2)
    {
        phase_name = "queue";
        queue_case();
    }
    if (cases & 4)
    {
        phase_name = "input";
        key_case();
    }
    /* the watchdog ends before the program does (a page's single thread
     * runs the others only when they wake) */
    SetEvent(quit);
    WaitForSingleObject(dog, INFINITE);
    printf("%d failures\n", failures);
    return failures != 0;
}
