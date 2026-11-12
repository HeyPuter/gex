/* splitlock: Windows' locks and interlocked calls on objects at addresses
 * that are not a multiple of their fields' size, which x86 allows (a
 * locked access there is a split lock) and a program may use: two threads
 * contend on each, natively in Wine's own code; and (32-bit) one thread's
 * locked increments in its own code race the other's calls of kernel32's
 * InterlockedIncrement on the same counter; and (32-bit) one thread's
 * locked increments in its own code of a counter across two 8-byte words
 * race the other's InterlockedIncrement calls, for as long, on the aligned
 * counter that overlaps its upper half (the second word's first four
 * bytes).
 *
 * Says "splitlock OFF what COUNT" for each, COUNT what two threads of N
 * rounds each made (2N when nothing was lost); for the last, "splitlock
 * 6 overlap kept" when both counters have all their rounds (M of the
 * split one's, the other's carried into the overlapping one). */
#include <windows.h>
#include <stdio.h>

#define N 20000
#define M 1000000

static unsigned char __attribute__((aligned(16))) buf[4096];
static volatile LONG counter;
static void *obj;
static int kind;
static LONG (WINAPI *xinc)(LONG volatile *);
static volatile LONG *mixed;
static volatile LONG *over;
static volatile LONG started, done, rounds;

static BOOL CALLBACK once_cb(INIT_ONCE *io, void *param, void **ctx)
{
    InterlockedIncrement(&counter);
    return TRUE;
}

static DWORD WINAPI worker(void *arg)
{
    int i, own = (int)(INT_PTR)arg;
    if (kind == 4)
    {
        /* the other thread's, from the first of this one's to its last */
        if (own)
        {
            InterlockedExchange(&started, 1);
            for (i = 0; !done; i++) xinc(over);
            rounds = i;
            return 0;
        }
        while (!started) YieldProcessor();
        for (i = 0; i < M; i++)
        {
#if defined(__i386__) || defined(__x86_64__)
            __asm__ __volatile__("lock incl %0" : "+m" (*mixed) :: "memory");
#endif
        }
        InterlockedExchange(&done, 1);
        return 0;
    }
    for (i = 0; i < N; i++)
    {
        switch (kind)
        {
        case 0:
            EnterCriticalSection(obj);
            counter++;
            LeaveCriticalSection(obj);
            break;
        case 1:
            AcquireSRWLockExclusive(obj);
            counter++;
            ReleaseSRWLockExclusive(obj);
            break;
        case 2:
            InitOnceExecuteOnce(obj, once_cb, NULL, NULL);
            break;
        case 3:
            /* one thread in Wine's code, the other in its own */
            if (own) xinc(mixed);
#if defined(__i386__) || defined(__x86_64__)
            else __asm__ __volatile__("lock incl %0" : "+m" (*mixed) :: "memory");
#endif
            break;
        }
    }
    return 0;
}

static void run(int k, unsigned off, const char *what)
{
    HANDLE t[2];
    int i;
    memset(buf, 0, sizeof(buf));
    obj = buf + off;
    kind = k;
    counter = 0;
    if (k == 0) InitializeCriticalSection(obj);
    if (k == 1) InitializeSRWLock(obj);
    if (k == 2) InitOnceInitialize(obj);
    if (k >= 3) mixed = (volatile LONG *)obj;
    over = (volatile LONG *)(buf + 8);
    started = done = 0;
    for (i = 0; i < 2; i++) t[i] = CreateThread(NULL, 0, worker, (void *)(INT_PTR)i, 0, NULL);
    WaitForMultipleObjects(2, t, TRUE, INFINITE);
    for (i = 0; i < 2; i++) CloseHandle(t[i]);
    if (k == 0) DeleteCriticalSection(obj);
    if (k == 4)
        printf("splitlock %u %s %s (%ld of %ld, %ld of %ld)\n", off, what,
               (*mixed & 0xffff) == (M & 0xffff) && *over == rounds + (M >> 16) ? "kept" : "lost",
               *mixed & 0xffff, (long)(M & 0xffff), *over - (M >> 16), rounds);
    else
        printf("splitlock %u %s %ld\n", off, what, k == 3 ? *mixed : counter);
    fflush(stdout);
}

int main(void)
{
    /* LockCount (offset 4 on i386, 8 on x64): at one of these inside one 8-byte word, at the other across two */
    run(0, 2, "critsec");
    run(0, 6, "critsec");
    run(1, 2, "srwlock");
    run(1, 6, "srwlock");
    run(2, 3, "initonce");
    if (sizeof(void *) == 4)
    {
        xinc = (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "InterlockedIncrement");
        if (xinc)
        {
            run(3, 1, "mixed");
            run(3, 6, "mixed");
            run(4, 6, "overlap");
        }
    }
    printf("splitlock done\n");
    return 0;
}
