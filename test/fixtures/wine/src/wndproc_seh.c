/* Exceptions in window procedures entered through user callbacks: one
 * raised and caught inside the procedure, one in a procedure a
 * callback's own SendMessage entered, and a context resumed
 * by a vectored handler inside a callback. After each the sender's stack
 * pointer and the window's state are as before.
 * x86-64 only (clang has no __try for i686 here, as seh.c); the i686 build
 * says so and passes.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld -municode */
#include <windows.h>
#include <stdio.h>

#ifndef __x86_64__
int wmain(void) { printf("ok not on this target\n0 failures\n"); return 0; }
#else

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static HWND win;
static int caught, inner_caught, resumed;
static volatile int resume_flag;

static LONG CALLBACK veh(EXCEPTION_POINTERS *e) {
    if (e->ExceptionRecord->ExceptionCode != 0xe0000005) return EXCEPTION_CONTINUE_SEARCH;
    resume_flag = 1;
    return EXCEPTION_CONTINUE_EXECUTION;
}

static void raise_caught(void) {
    __try { RaiseException(0xe0000001, 0, 0, 0); } __except (GetExceptionCode() == 0xe0000001) { caught++; }
}
static void raise_caught_inner(void) {
    __try { RaiseException(0xe0000003, 0, 0, 0); } __except (EXCEPTION_EXECUTE_HANDLER) { inner_caught++; }
}

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    switch (m) {
    case WM_USER:
        raise_caught();
        return 11;
    case WM_USER + 2:
        /* a callback inside this one, which raises and catches */
        return SendMessageW(w, WM_USER + 3, 0, 0) + 100;
    case WM_USER + 3:
        raise_caught_inner();
        return 13;
    case WM_USER + 4:
        RaiseException(0xe0000005, 0, 0, 0);
        resumed = resume_flag;
        return 14;
    }
    return DefWindowProcW(w, m, wp, lp);
}

static __attribute__((noinline)) ULONG_PTR sp_now(void) { return (ULONG_PTR)__builtin_frame_address(0); }

int wmain(void) {
    WNDCLASSW wc = { .lpfnWndProc = proc, .hInstance = GetModuleHandleW(0), .lpszClassName = L"wndproc_seh" };
    CHECK("RegisterClass", RegisterClassW(&wc) != 0);
    win = CreateWindowExW(0, L"wndproc_seh", L"t", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, 0, 0, wc.hInstance, 0);
    CHECK("CreateWindowEx", win != 0);
    AddVectoredExceptionHandler(1, veh);
    for (int round = 0; round < 3; round++) {
        ULONG_PTR sp0 = sp_now();
        LRESULT r = SendMessageW(win, WM_USER, 0, 0);
        CHECK("caught in the procedure", r == 11 && caught == round + 1 && sp_now() == sp0);
        r = SendMessageW(win, WM_USER + 2, 0, 0);
        CHECK("caught in a nested callback", r == 113 && inner_caught == round + 1 && sp_now() == sp0);
        resume_flag = 0; resumed = 0;
        r = SendMessageW(win, WM_USER + 4, 0, 0);
        CHECK("resumed by a vectored handler", r == 14 && resumed && sp_now() == sp0);
        CHECK("window still there", IsWindow(win) && GetWindowLongPtrW(win, GWLP_WNDPROC) == (LONG_PTR)proc);
    }
    CHECK("DestroyWindow", DestroyWindow(win));
    printf("%d failures\n", failures);
    return failures != 0;
}
#endif
