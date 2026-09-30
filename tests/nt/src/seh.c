/* Structured exceptions: __try/__except and __finally, RaiseException with
 * parameters, nested handlers and unwinding, continuing from a vectored
 * handler, processor exceptions (int3, ud2, executing data), and an
 * unhandled exception reaching the top-level filter.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

#ifdef __i386__
/* i386 has no table-based SEH: handlers are EXCEPTION_REGISTRATION records
 * chained from fs:0, walked by ntdll's RtlDispatchException. clang's __try
 * needs the MSVC target there, so the frames are built by hand:
 * guarded_call(fn) registers `frame_handler`, calls fn, and returns 1 if an
 * exception resumed it through a modified CONTEXT (NtContinue), 0 if fn
 * returned. */
static volatile DWORD caught_code, caught_info0, caught_info1;
__attribute__((used)) DWORD recovery_esp, recovery_ebp, recovery_eip;

__attribute__((used)) EXCEPTION_DISPOSITION __cdecl frame_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *dispatch) {
    if (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)) return ExceptionContinueSearch;
    caught_code = rec->ExceptionCode;
    caught_info0 = rec->NumberParameters > 0 ? rec->ExceptionInformation[0] : 0;
    caught_info1 = rec->NumberParameters > 1 ? rec->ExceptionInformation[1] : 0;
    ctx->Esp = recovery_esp;
    ctx->Ebp = recovery_ebp;
    ctx->Eip = recovery_eip;
    return ExceptionContinueExecution;
}

int guarded_call(void (*fn)(void));
__asm__(".globl _guarded_call\n"
        "_guarded_call:\n\t"
        "pushl %ebp\n\tmovl %esp, %ebp\n\tpushl %ebx\n\tpushl %esi\n\tpushl %edi\n\t"
        "pushl $_frame_handler\n\tpushl %fs:0\n\tmovl %esp, %fs:0\n\t"
        "movl %esp, _recovery_esp\n\tmovl %ebp, _recovery_ebp\n\tmovl $1f, _recovery_eip\n\t"
        "call *8(%ebp)\n\t"
        "xorl %eax, %eax\n\tjmp 2f\n"
        "1:\tmovl $1, %eax\n"
        "2:\tmovl (%esp), %edx\n\tmovl %edx, %fs:0\n\taddl $8, %esp\n\t"
        "popl %edi\n\tpopl %esi\n\tpopl %ebx\n\tpopl %ebp\n\tret\n");

static void raise_it(void) { ULONG_PTR a[2] = {7, 0x1234}; RaiseException(0xe0001234, 0, 2, a); }
static void nothing(void) {}
__attribute__((noinline)) static void do_int3(void) { __asm__ volatile("int3\n\tnop"); }
__attribute__((noinline)) static void do_ud2(void) { __asm__ volatile("ud2"); }
static unsigned char data_bytes[16] = {0xc3};
static void exec_data(void) { ((void (*)(void))(void *)data_bytes)(); }
#endif

#ifdef __x86_64__
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

/* In functions of their own: an instruction that ends a __try block's code
 * can fall outside the block's scope range (clang ends it at the last
 * call), so the handler is found in the caller, as it would be for any
 * faulting callee. */
__attribute__((noinline)) static void do_int3(void) { __debugbreak(); __asm__ volatile("nop"); }
__attribute__((noinline)) static void do_ud2(void) { __builtin_trap(); }
#endif

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
#ifdef __x86_64__
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

#else
    CHECK("frame handler catches RaiseException", guarded_call(raise_it) == 1 && caught_code == 0xe0001234 && caught_info1 == 0x1234);
    CHECK("no exception, no handler", guarded_call(nothing) == 0);
    CHECK("int3 is EXCEPTION_BREAKPOINT", guarded_call(do_int3) == 1 && caught_code == EXCEPTION_BREAKPOINT);
    CHECK("ud2 is EXCEPTION_ILLEGAL_INSTRUCTION", guarded_call(do_ud2) == 1 && caught_code == EXCEPTION_ILLEGAL_INSTRUCTION);
    CHECK("executing data is an access violation", guarded_call(exec_data) == 1 && caught_code == EXCEPTION_ACCESS_VIOLATION && caught_info0 == 8 && caught_info1 == (DWORD)data_bytes);
#endif

    PVOID h = AddVectoredExceptionHandler(1, vectored);
    RaiseException(0xe0005678, 0, 0, 0);
    RaiseException(0xe0005678, 0, 0, 0);
    CHECK("vectored handler continues execution", vectored_calls == 2);
    RemoveVectoredExceptionHandler(h);

#ifdef __x86_64__
    DWORD code = 0;
    __try { do_int3(); } __except (code = GetExceptionCode(), EXCEPTION_EXECUTE_HANDLER) {}
    CHECK("int3 is EXCEPTION_BREAKPOINT", code == EXCEPTION_BREAKPOINT);
    code = 0;
    __try { do_ud2(); } __except (code = GetExceptionCode(), EXCEPTION_EXECUTE_HANDLER) {}
    CHECK("ud2 is EXCEPTION_ILLEGAL_INSTRUCTION", code == EXCEPTION_ILLEGAL_INSTRUCTION);
    static unsigned char data_bytes[16] = {0xc3};
    ULONG_PTR kind = 0, where = 0;
    code = 0;
    __try { ((void (*)(void))(void *)data_bytes)(); }
    __except (code = GetExceptionCode(),
              kind = GetExceptionInformation()->ExceptionRecord->ExceptionInformation[0],
              where = GetExceptionInformation()->ExceptionRecord->ExceptionInformation[1],
              EXCEPTION_EXECUTE_HANDLER) {}
    CHECK("executing data is an access violation", code == EXCEPTION_ACCESS_VIOLATION && kind == 8 && where == (ULONG_PTR)data_bytes);
#endif

    SetUnhandledExceptionFilter(top_filter);
    RaiseException(0xe00000ff, EXCEPTION_NONCONTINUABLE, 0, 0);
    printf("FAIL returned from an unhandled exception\n");
    return 1;
}
