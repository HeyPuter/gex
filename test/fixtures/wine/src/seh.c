/* Structured exceptions: __try/__except and __finally, RaiseException with
 * parameters, nested handlers and unwinding, continuing from a vectored
 * handler, processor exceptions (int3, ud2, executing data), and an
 * unhandled exception reaching the top-level filter.
 * `seh.exe traps`: divide errors and `hlt` instead (EXCEPTION_INT_DIVIDE_BY_ZERO,
 * EXCEPTION_INT_OVERFLOW, EXCEPTION_PRIV_INSTRUCTION), then exits.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fms-extensions -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <string.h>

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

/* Processor traps, each in a function of its own. */
__attribute__((noinline)) static void do_div0(void) {
    __asm__ volatile("xorl %%edx, %%edx\n\tmovl $1, %%eax\n\txorl %%ecx, %%ecx\n\tdivl %%ecx" ::: "eax", "ecx", "edx");
}
__attribute__((noinline)) static void do_idiv_overflow(void) {
    __asm__ volatile("movl $0x80000000, %%eax\n\tcltd\n\tmovl $-1, %%ecx\n\tidivl %%ecx" ::: "eax", "ecx", "edx");
}
__attribute__((noinline)) static void do_hlt(void) { __asm__ volatile("hlt"); }

static int traps(void) {
#ifdef __x86_64__
    DWORD code = 0;
    __try { do_div0(); } __except (code = GetExceptionCode(), EXCEPTION_EXECUTE_HANDLER) {}
    CHECK("div by zero is EXCEPTION_INT_DIVIDE_BY_ZERO", code == EXCEPTION_INT_DIVIDE_BY_ZERO);
    code = 0;
    __try { do_idiv_overflow(); } __except (code = GetExceptionCode(), EXCEPTION_EXECUTE_HANDLER) {}
    /* Windows reports a quotient overflow as EXCEPTION_INT_OVERFLOW (native
     * Wine says INT_DIVIDE_BY_ZERO: Linux gives it FPE_INTDIV). */
    CHECK("INT_MIN / -1 is EXCEPTION_INT_OVERFLOW", code == EXCEPTION_INT_OVERFLOW);
    code = 0;
    __try { do_hlt(); } __except (code = GetExceptionCode(), EXCEPTION_EXECUTE_HANDLER) {}
    CHECK("hlt is EXCEPTION_PRIV_INSTRUCTION", code == EXCEPTION_PRIV_INSTRUCTION);
#else
    CHECK("div by zero is EXCEPTION_INT_DIVIDE_BY_ZERO", guarded_call(do_div0) == 1 && caught_code == EXCEPTION_INT_DIVIDE_BY_ZERO);
    CHECK("INT_MIN / -1 is EXCEPTION_INT_OVERFLOW", guarded_call(do_idiv_overflow) == 1 && caught_code == EXCEPTION_INT_OVERFLOW);
    CHECK("hlt is EXCEPTION_PRIV_INSTRUCTION", guarded_call(do_hlt) == 1 && caught_code == EXCEPTION_PRIV_INSTRUCTION);
#endif
    printf("%d failures\n", failures);
    return failures;
}

static volatile LONG vectored_calls;
static LONG CALLBACK vectored(EXCEPTION_POINTERS *e) {
    if (e->ExceptionRecord->ExceptionCode != 0xe0005678) return EXCEPTION_CONTINUE_SEARCH;
    InterlockedIncrement(&vectored_calls);
    return EXCEPTION_CONTINUE_EXECUTION;
}

/* A vectored handler that continues past an int3, as a debugger does:
 * the faulting function goes on at the next instruction. */
static volatile LONG bp_calls;
static LONG CALLBACK bp_vectored(EXCEPTION_POINTERS *e) {
    if (e->ExceptionRecord->ExceptionCode != EXCEPTION_BREAKPOINT) return EXCEPTION_CONTINUE_SEARCH;
    InterlockedIncrement(&bp_calls);
#ifdef __x86_64__
    e->ContextRecord->Rip = (DWORD64)e->ExceptionRecord->ExceptionAddress + 1;
#else
    e->ContextRecord->Eip = (DWORD)e->ExceptionRecord->ExceptionAddress + 1;
#endif
    return EXCEPTION_CONTINUE_EXECUTION;
}
__attribute__((noinline)) static int past_int3(volatile int *p) {
    int a = *p;
    __asm__ volatile("int3");
    return a + *p + 1;
}

__attribute__((noinline)) static int raise_keeps_control_words(int pad) {
    volatile char *room = __builtin_alloca(pad + 1);
    room[0] = 0;
    unsigned short cw = 0x0c7f, cw_after;
    unsigned int mxcsr = 0x7f80, mxcsr_after, mxcsr_before;
    __asm__ volatile("stmxcsr %0" : "=m"(mxcsr_before));
    __asm__ volatile("fldcw %0; ldmxcsr %1" :: "m"(cw), "m"(mxcsr));
    RaiseException(0xe0005678, 0, 0, 0);
    __asm__ volatile("fnstcw %0; stmxcsr %1" : "=m"(cw_after), "=m"(mxcsr_after));
    cw = 0x027f;
    __asm__ volatile("fldcw %0; ldmxcsr %1" :: "m"(cw), "m"(mxcsr_before));
    return (cw_after & ~0x40) == 0x0c3f && mxcsr_after == 0x7f80;
}

static LONG WINAPI top_filter(EXCEPTION_POINTERS *e) {
    printf("top-level filter saw %08lx\n", e->ExceptionRecord->ExceptionCode);
    printf("%d failures\n", failures);
    fflush(stdout);
    ExitProcess(failures ? failures : 77);
    return EXCEPTION_EXECUTE_HANDLER;
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "traps")) return traps();
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
    /* Continuing restores the FPU control words the raise captured, at
     * every stack alignment (the dispatcher's frame overlaps the CONTEXT
     * RtlRaiseException keeps below the caller's stack). */
    int kept = 1;
    for (int pad = 0; pad < 64; pad += 4) kept &= raise_keeps_control_words(pad);
    CHECK("continuing keeps the x87 and SSE control words", kept);
    RemoveVectoredExceptionHandler(h);
    h = AddVectoredExceptionHandler(1, bp_vectored);
    volatile int twenty = 20;
    int sum = 0;
    for (int i = 0; i < 3; i++) sum += past_int3(&twenty);
    CHECK("a vectored handler continues past int3", sum == 3 * 41 && bp_calls == 3);
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
              kind = ((EXCEPTION_POINTERS *)GetExceptionInformation())->ExceptionRecord->ExceptionInformation[0],
              where = ((EXCEPTION_POINTERS *)GetExceptionInformation())->ExceptionRecord->ExceptionInformation[1],
              EXCEPTION_EXECUTE_HANDLER) {}
    CHECK("executing data is an access violation", code == EXCEPTION_ACCESS_VIOLATION && kind == 8 && where == (ULONG_PTR)data_bytes);
#endif

    SetUnhandledExceptionFilter(top_filter);
    RaiseException(0xe00000ff, EXCEPTION_NONCONTINUABLE, 0, 0);
    printf("FAIL returned from an unhandled exception\n");
    return 1;
}
