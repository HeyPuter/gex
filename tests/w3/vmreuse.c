/* Code at reused addresses from several threads (vmreuse.sh): each thread
 * allocates a page, writes `mov eax, imm32; ret`, makes it executable,
 * calls it and frees it, while the others do the same, so a freed page is
 * soon another thread's with other code in it. The NT layer's free and its
 * retire of the old translations are separate kernel-lock sections; the
 * call must still return the value just written (docs/kernel-locking.md
 * §4.3, L12b). Prints "vmreuse ok N". */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#define ROUNDS 300
#define THREADS 4
static volatile LONG bad;

static DWORD WINAPI worker(LPVOID arg) {
    int id = (int)(INT_PTR)arg;
    for (int i = 0; i < ROUNDS; i++) {
        unsigned char *p = VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if (!p) { InterlockedIncrement(&bad); continue; }
        LONG want = id * 100000 + i;
        p[0] = 0xb8;
        memcpy(p + 1, &want, 4);
        p[5] = 0xc3;
        DWORD old;
        if (!VirtualProtect(p, 4096, PAGE_EXECUTE_READ, &old)) { InterlockedIncrement(&bad); continue; }
        LONG got = ((LONG (*)(void))p)();
        if (got != want) {
            if (InterlockedIncrement(&bad) < 5) printf("thread %d round %d: stale code: %ld, wanted %ld\n", id, i, got, want);
        }
        if (!VirtualFree(p, 0, MEM_RELEASE)) InterlockedIncrement(&bad);
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
