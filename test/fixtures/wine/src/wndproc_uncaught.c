/* Exceptions a window procedure leaves uncaught, raised ones and divide
 * errors, on both ways USER enters a procedure.
 * A message USER delivers through a user callback (SetWindowPos's
 * WM_WINDOWPOSCHANGING, DestroyWindow's WM_DESTROY) runs under
 * KiUserCallbackDispatcher, whose handler ends the callback: the call that
 * sent it returns, with the caller's stack pointer as before, and the
 * process's top-level filter never sees the exception (it would end the
 * process, exit 99). Nested callbacks end one at a time.
 * A same-thread SendMessageW calls the procedure directly, so its exception
 * unwinds into the sender, whose own handler catches it.
 * A caught one inside a procedure still reaches its own handler.
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

enum { NONE, RAISE, FAULT, NESTED };
static volatile int mode, entered, caught, after;
__attribute__((noinline)) static void do_div0(void) {
    __asm__ volatile("xorl %%edx, %%edx\n\tmovl $1, %%eax\n\txorl %%ecx, %%ecx\n\tdivl %%ecx" ::: "eax", "ecx", "edx");
}

static LONG WINAPI top(EXCEPTION_POINTERS *e) {
    printf("FAIL top-level filter saw %lx\n", e->ExceptionRecord->ExceptionCode);
    fflush(stdout);
    ExitProcess(99);
}

static void fail_now(int how) {
    entered++;
    if (how == RAISE) RaiseException(0xe0000010, 0, 0, 0);
    else do_div0();
    after++;
}

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM wp, LPARAM lp) {
    switch (m) {
    case WM_WINDOWPOSCHANGING:
        if (mode == NESTED) {
            /* a callback inside this one fails; this one carries on */
            mode = RAISE;
            SetWindowPos(w, 0, 0, 0, 30, 30, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
            entered += 100;
            return 0;
        }
        if (mode != NONE) fail_now(mode);
        break;
    case WM_DESTROY:
        if (mode != NONE) fail_now(mode);
        break;
    case WM_USER:
        fail_now(RAISE);
        return 21;
    case WM_USER + 1:
        fail_now(FAULT);
        return 22;
    case WM_USER + 3:
        __try { RaiseException(0xe0000011, 0, 0, 0); } __except (EXCEPTION_EXECUTE_HANDLER) { caught++; }
        return 23;
    }
    return DefWindowProcW(w, m, wp, lp);
}

static __attribute__((noinline)) ULONG_PTR sp_now(void) { return (ULONG_PTR)__builtin_frame_address(0); }

static HWND make(HINSTANCE inst) {
    return CreateWindowExW(0, L"wndproc_uncaught", L"t", WS_OVERLAPPEDWINDOW, 0, 0, 100, 100, 0, 0, inst, 0);
}

static __attribute__((noinline)) DWORD send_caught(HWND w, UINT m) {
    __try { SendMessageW(w, m, 0, 0); } __except (EXCEPTION_EXECUTE_HANDLER) { return GetExceptionCode(); }
    return 0;
}

static __attribute__((noinline)) void resize(HWND w, int how, int size) {
    mode = how;
    SetWindowPos(w, 0, 0, 0, size, size, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
    mode = NONE;
}

int wmain(void) {
    SetUnhandledExceptionFilter(top);
    SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
    HINSTANCE inst = GetModuleHandleW(0);
    WNDCLASSW wc = { .lpfnWndProc = proc, .hInstance = inst, .lpszClassName = L"wndproc_uncaught" };
    CHECK("RegisterClass", RegisterClassW(&wc) != 0);
    HWND win = make(inst);
    CHECK("CreateWindowEx", win != 0);
    for (int round = 0; round < 3; round++) {
        ULONG_PTR sp0 = sp_now();
        int e0 = entered, a0 = after;
        resize(win, RAISE, 40 + round);
        CHECK("a raised exception ends its user callback", entered == e0 + 1 && sp_now() == sp0);
        resize(win, FAULT, 50 + round);
        CHECK("a divide error ends its user callback", entered == e0 + 2 && sp_now() == sp0);
        resize(win, NESTED, 60 + round);
        CHECK("a nested user callback's ends only that one", entered == e0 + 103 && sp_now() == sp0);
        CHECK("nothing ran past a fault", after == a0);
        CHECK("window still there", IsWindow(win) && GetWindowLongPtrW(win, GWLP_WNDPROC) == (LONG_PTR)proc);

        CHECK("same-thread send: the sender catches a raised one", send_caught(win, WM_USER) == 0xe0000010 && sp_now() == sp0);
        CHECK("same-thread send: the sender catches a divide error", send_caught(win, WM_USER + 1) == EXCEPTION_INT_DIVIDE_BY_ZERO && sp_now() == sp0);
        CHECK("a caught one still reaches its handler", SendMessageW(win, WM_USER + 3, 0, 0) == 23 && caught == round + 1);
        CHECK("nothing ran past a fault", after == a0);

        HWND gone = make(inst);
        mode = round & 1 ? FAULT : RAISE;
        e0 = entered;
        CHECK("DestroyWindow with a failing WM_DESTROY", gone && DestroyWindow(gone) && entered == e0 + 1 && !IsWindow(gone) && sp_now() == sp0);
        mode = NONE;
    }
    CHECK("DestroyWindow", DestroyWindow(win));
    printf("%d failures\n", failures);
    return failures != 0;
}
#endif
