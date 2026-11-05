/* Code at reused addresses from several threads (test/wow64/vmreuse): each thread
 * allocates a page, writes `mov eax, imm32; ret`, makes it executable,
 * calls it and frees it, while the others do the same, so a freed page is
 * soon another thread's with other code in it. The NT layer's free and its
 * retire of the old translations are separate kernel-lock sections; the
 * call must still return the value just written (docs/kernel-locking.md
 * §4.3). Prints "vmreuse ok N". */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#define ROUNDS 300
#define THREADS 4
static volatile LONG bad;

/* A whole line, flushed: the reason is what a failure has to show. */
#define say(...) (printf(__VA_ARGS__), fflush(stdout))

static DWORD WINAPI worker(LPVOID arg) {
    int id = (int)(INT_PTR)arg;
    for (int i = 0; i < ROUNDS; i++) {
        unsigned char *p = VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if (!p) { if (InterlockedIncrement(&bad) < 5) say("thread %d round %d: VirtualAlloc failed %lu\n", id, i, GetLastError()); continue; }
        LONG want = id * 100000 + i;
        p[0] = 0xb8;
        memcpy(p + 1, &want, 4);
        p[5] = 0xc3;
        DWORD old;
        if (!VirtualProtect(p, 4096, PAGE_EXECUTE_READ, &old)) { if (InterlockedIncrement(&bad) < 5) say("thread %d round %d: VirtualProtect failed %lu\n", id, i, GetLastError()); continue; }
        LONG got = ((LONG (*)(void))p)();
        if (got != want) {
            if (InterlockedIncrement(&bad) < 5) say("thread %d round %d: stale code at %p: %ld, wanted %ld\n", id, i, p, got, want);
        }
        if (!VirtualFree(p, 0, MEM_RELEASE) && InterlockedIncrement(&bad) < 5) say("thread %d round %d: VirtualFree failed %lu\n", id, i, GetLastError());
    }
    return 0;
}

int main(void) {
    HANDLE t[THREADS];
    for (int i = 0; i < THREADS; i++) t[i] = CreateThread(NULL, 0, worker, (LPVOID)(INT_PTR)i, 0, NULL);
    WaitForMultipleObjects(THREADS, t, TRUE, INFINITE);
    if (bad) {
        printf("vmreuse FAILED %ld\n", bad);
        return 1;
    }
    printf("vmreuse ok %d\n", ROUNDS * THREADS);
    fflush(stdout);
    return 0;
}
