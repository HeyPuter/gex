/* Structured exceptions: __try/__except and __finally, RaiseException with
 * parameters, nested handlers and unwinding, continuing from a vectored
 * handler, and an unhandled exception reaching the top-level filter.
 * gex-nt-cc: clang --target=x86_64-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static int finally_ran;
static ULONG_PTR seen_param;
static int filter(EXCEPTION_POINTERS *e) {
    seen_param = e->ExceptionRecord->NumberParameters == 2 ? e->ExceptionRecord->ExceptionInformation[1] : 0;
    return EXCEPTION_EXECUTE_HANDLER;
}

__attribute__((noinline)) static void inner(void) {
    __try {
        ULONG_PTR args[2] = {7, 0x1234};
        RaiseException(0xe0001234, 0, 2, args);
    } __finally {
        finally_ran = 1;
    }
}

static volatile LONG vectored_calls;
static LONG CALLBACK vectored(EXCEPTION_POINTERS *e) {
    if (e->ExceptionRecord->ExceptionCode != 0xe0005678) return EXCEPTION_CONTINUE_SEARCH;
    InterlockedIncrement(&vectored_calls);
    return EXCEPTION_CONTINUE_EXECUTION;
}

static LONG WINAPI top_filter(EXCEPTION_POINTERS *e) {
    printf("top-level filter saw %08lx\n", e->ExceptionRecord->ExceptionCode);
    printf("%d failures\n", failures);
    fflush(stdout);
    ExitProcess(failures ? failures : 77);
    return EXCEPTION_EXECUTE_HANDLER;
}

int main(void) {
    int caught = 0;
    __try { RaiseException(0xe0000001, 0, 0, 0); } __except (GetExceptionCode() == 0xe0000001) { caught = 1; }
    CHECK("__try/__except", caught);

    caught = 0;
    __try { inner(); } __except (filter(GetExceptionInformation())) { caught = GetExceptionCode() == 0xe0001234; }
    CHECK("nested: unwound through __finally", caught && finally_ran && seen_param == 0x1234);

    int depth = 0;
    __try {
        __try { RaiseException(0xe0000002, 0, 0, 0); }
        __except (EXCEPTION_CONTINUE_SEARCH) { depth = 1; }
    } __except (EXCEPTION_EXECUTE_HANDLER) { depth = 2; }
    CHECK("CONTINUE_SEARCH reaches the outer handler", depth == 2);

    PVOID h = AddVectoredExceptionHandler(1, vectored);
    RaiseException(0xe0005678, 0, 0, 0);
    RaiseException(0xe0005678, 0, 0, 0);
    CHECK("vectored handler continues execution", vectored_calls == 2);
    RemoveVectoredExceptionHandler(h);

    SetUnhandledExceptionFilter(top_filter);
    RaiseException(0xe00000ff, EXCEPTION_NONCONTINUABLE, 0, 0);
    printf("FAIL returned from an unhandled exception\n");
    return 1;
}
