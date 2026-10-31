/* Thread-local storage: TlsAlloc slots, the image's own .tls data
 * (__declspec(thread)) in every thread, and FLS callbacks at thread exit.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

__declspec(thread) int tls_counter = 100;
__declspec(thread) char tls_buf[64] = "initial";
static DWORD slot, fls;
static volatile LONG fls_freed;
static void WINAPI fls_cb(void *p) { if (p) InterlockedIncrement(&fls_freed); }

static DWORD WINAPI worker(void *p) {
    int id = (int)(ULONG_PTR)p;
    int ok = tls_counter == 100 && strcmp(tls_buf, "initial") == 0 && TlsGetValue(slot) == 0;
    tls_counter += id;
    sprintf(tls_buf, "thread %d", id);
    TlsSetValue(slot, (void *)(ULONG_PTR)(id * 10));
    FlsSetValue(fls, (void *)1);
    Sleep(10);
    ok = ok && tls_counter == 100 + id && TlsGetValue(slot) == (void *)(ULONG_PTR)(id * 10);
    char want[16]; sprintf(want, "thread %d", id);
    ok = ok && strcmp(tls_buf, want) == 0;
    return ok;
}

int main(void) {
    slot = TlsAlloc();
    fls = FlsAlloc(fls_cb);
    CHECK("TlsAlloc", slot != TLS_OUT_OF_INDEXES);
    TlsSetValue(slot, (void *)42);
    tls_counter = 1;
    HANDLE t[4];
    for (int i = 0; i < 4; i++) t[i] = CreateThread(0, 0, worker, (void *)(ULONG_PTR)(i + 1), 0, 0);
    WaitForMultipleObjects(4, t, TRUE, INFINITE);
    int all = 1;
    for (int i = 0; i < 4; i++) { DWORD c = 0; GetExitCodeThread(t[i], &c); all &= c == 1; CloseHandle(t[i]); }
    CHECK("each thread has its own TLS", all);
    CHECK("main thread's TLS untouched", tls_counter == 1 && TlsGetValue(slot) == (void *)42 && strcmp(tls_buf, "initial") == 0);
    CHECK("FLS callbacks ran at thread exit", fls_freed == 4);
    CHECK("TlsFree", TlsFree(slot));
    printf("%d failures\n", failures);
    return failures;
}
