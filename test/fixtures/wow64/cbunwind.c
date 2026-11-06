/* An exception thrown in a window procedure and caught above the
 * SendMessage that called it, as C++Builder's throw and catch do
 * (test/wow64/cbunwind): an SEH frame registered around the call, a
 * handler that unwinds to it (RtlUnwind) and goes on in its function with
 * that function's stack pointer, as a catch block does. Between the two
 * stand user32's frames calling the procedure, native ones where i386
 * Wine is native wasm32; they are left behind, never returned to.
 *
 * N rounds (argv[1], default 50), each a catch, with a message that
 * returns normally between them, and in round 3 a second window procedure
 * nested inside the first, so the unwind goes past two callbacks. Then the
 * same catch made DEEP messages down, each sent by win32u from inside the
 * procedure of the one before (a window that moves itself while it is
 * being moved): a user callback into the 32-bit code for each, as nested
 * as WinSCP's modal loops have them. Prints "cbunwind ok N" and exits 0. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

#define CODE 0x0EEDFADE

struct frame
{
    EXCEPTION_REGISTRATION_RECORD reg;
    void *jb[5];  /* __builtin_setjmp's: the frame and stack pointers, where to go on */
};

#define DEEP 12

static HWND hwnd;
static int depth, level, deep_caught = -1;

static int send_and_catch(int nest);

static LRESULT CALLBACK proc(HWND w, UINT msg, WPARAM wp, LPARAM lp)
{
    if (msg == WM_USER)
    {
        /* (twice: the second procedure runs inside the first) */
        if (wp && depth++ < 1) SendMessageA(w, WM_USER, 0, 0);
        RaiseException(CODE, 0, 0, NULL);
        return -1;  /* not reached */
    }
    if (msg == WM_USER + 1) return lp + 1;
    /* the nest of moves: each level's move from the one before's */
    if (msg == WM_WINDOWPOSCHANGING && level && deep_caught < 0)
    {
        if (level < DEEP)
        {
            level++;
            SetWindowPos(w, 0, 0, 0, 10 + level, 10 + level, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE);
        }
        else deep_caught = send_and_catch(0);
        return 0;
    }
    return DefWindowProcA(w, msg, wp, lp);
}

static EXCEPTION_DISPOSITION __cdecl __attribute__((noinline)) handler(EXCEPTION_RECORD *rec, void *reg, CONTEXT *ctx, void *disp)
{
    struct frame *f = reg;

    if (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)) return ExceptionContinueSearch;
    if (rec->ExceptionCode != CODE) return ExceptionContinueSearch;
    RtlUnwind(reg, NULL, rec, NULL);
    /* the catch: on in its own function, with that function's stack */
    __builtin_longjmp(f->jb, 1);
}

/* Returns 1 when the window procedure's exception was caught here. */
static int __attribute__((noinline)) send_and_catch(int nest)
{
    struct frame f;
    volatile int caught = 0;

    f.reg.Handler = (PEXCEPTION_ROUTINE)handler;
    f.reg.Next = (EXCEPTION_REGISTRATION_RECORD *)__readfsdword(0);
    __writefsdword(0, (DWORD)&f.reg);
    if (!__builtin_setjmp(f.jb))
        SendMessageA(hwnd, WM_USER, nest, 0);
    else
        caught = 1;
    __writefsdword(0, (DWORD)f.reg.Next);
    return caught;
}

int main(int argc, char **argv)
{
    int rounds = argc > 1 ? atoi(argv[1]) : 50, i, bad = 0;
    WNDCLASSA wc = { 0 };

    setvbuf(stdout, NULL, _IONBF, 0);

    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "cbunwind";
    if (!RegisterClassA(&wc)) { printf("cbunwind: RegisterClass failed (%lu)\n", GetLastError()); return 1; }
    hwnd = CreateWindowA("cbunwind", "cbunwind", 0, 0, 0, 10, 10, HWND_MESSAGE, NULL, wc.hInstance, NULL);
    if (!hwnd) { printf("cbunwind: CreateWindow failed (%lu)\n", GetLastError()); return 1; }

    for (i = 0; i < rounds; i++)
    {
        void *before = (void *)__readfsdword(0);
        depth = 0;
        if (getenv("CBUNWIND_TRACE")) printf("round %d\n", i);
        if (!send_and_catch(i == 3))
        {
            printf("cbunwind: round %d: not caught\n", i);
            bad++;
        }
        if ((void *)__readfsdword(0) != before)
        {
            printf("cbunwind: round %d: the frame chain is %p, not %p\n", i, (void *)__readfsdword(0), before);
            bad++;
        }
        if (SendMessageA(hwnd, WM_USER + 1, 0, i) != i + 1)
        {
            printf("cbunwind: round %d: a message after the catch came back wrong\n", i);
            bad++;
        }
        if (bad) break;
    }
    if (!bad)
    {
        level = 1;
        SetWindowPos(hwnd, 0, 0, 0, 10, 10, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE);
        if (deep_caught != 1)
        {
            printf("cbunwind: %d messages down: %s\n", DEEP, deep_caught < 0 ? "never got there" : "not caught");
            bad++;
        }
        else if (SendMessageA(hwnd, WM_USER + 1, 0, 7) != 8)
        {
            printf("cbunwind: %d messages down: a message after the catch came back wrong\n", DEEP);
            bad++;
        }
    }
    DestroyWindow(hwnd);
    if (bad) return 1;
    printf("cbunwind ok %d\n", rounds);
    return 0;
}
