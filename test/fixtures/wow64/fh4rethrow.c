/* `throw;` in a catch(...), as MSVC's x64 C++ runs it when the program
 * links its C++ runtime in (coreclr does): the frame handler finds the
 * catch and calls RtlUnwindEx with STATUS_UNWIND_CONSOLIDATE, and the
 * consolidate callback, the program's own code (__CxxCallCatchBlock), runs
 * the catch block, whose rethrow dispatches a new exception from inside the
 * callback. An outer catch two frames up takes it, so its dispatch goes
 * from the callback through RtlRestoreContext to the frames of the context
 * the callback was to resume (the inner function's), never back through
 * the frames the catch unwound: the inner try would take the rethrow again
 * from there (terminate(), for C++). A frame between the try and the raise
 * has a handler that says so. The frame handlers and the callback are
 * small C functions, and the functions they handle assembly with unwind
 * info naming them (MinGW has no SEH of its own).
 * Prints "fh4 rethrow caught" and exits 0, or "fh4 rethrow not caught" or
 * "fh4 rethrow went back through the frames the catch unwound" (exit 1);
 * an exception nothing catches ends the process instead. */
#include <windows.h>
#include <stdio.h>

#define FIRST 0x2000f4f4
#define AGAIN 0x2000f4f5

/* int guarded(void (*fn)(void), handler): calls fn; the handler named in
 * its unwind info may resume at its `_on` label with rax its result */
int inner_guarded(void (*fn)(void));
int mid_guarded(void (*fn)(void));
int outer_guarded(void (*fn)(void));
extern char inner_on[], mid_on[], outer_on[];

#define GUARDED(name)                                    \
    ".globl " #name "_guarded\n"                         \
    ".def " #name "_guarded; .scl 2; .type 32; .endef\n" \
    ".seh_proc " #name "_guarded\n"                      \
    #name "_guarded:\n"                                  \
    "    pushq %rbp\n"                                   \
    "    .seh_pushreg %rbp\n"                            \
    "    subq $48, %rsp\n"                               \
    "    .seh_stackalloc 48\n"                           \
    "    .seh_endprologue\n"                             \
    "    callq *%rcx\n"                                  \
    "    xorl %eax, %eax\n"                              \
    "    nop\n"                                          \
    ".globl " #name "_on\n"                              \
    #name "_on:\n"                                       \
    "    addq $48, %rsp\n"                               \
    "    popq %rbp\n"                                    \
    "    retq\n"                                         \
    "    .seh_handler " #name "_handler, @except\n"      \
    ".seh_endproc\n"

__asm__(".text\n" GUARDED(inner) GUARDED(mid) GUARDED(outer));

static int rethrown, seen_again;

/* the catch block: `throw;` */
static void *CALLBACK catch_block(EXCEPTION_RECORD *rec)
{
    (void)rec;
    rethrown = 1;
    RaiseException(AGAIN, 0, 0, NULL);
    return inner_on;
}

EXCEPTION_DISPOSITION inner_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, DISPATCHER_CONTEXT *dispatch)
{
    EXCEPTION_RECORD consolidate = { 0 };

    if (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND))
        return ExceptionContinueSearch;
    /* the rethrow comes here from the frame the callback resumes (at the
     * call in the try, as on Windows: a C++ handler knows it for its own
     * catch's rethrow), and goes on to the outer one */
    if (rec->ExceptionCode == AGAIN) return ExceptionContinueSearch;
    if (rec->ExceptionCode != FIRST)
        return ExceptionContinueSearch;
    consolidate.ExceptionCode = STATUS_UNWIND_CONSOLIDATE;
    consolidate.NumberParameters = 1;
    consolidate.ExceptionInformation[0] = (ULONG_PTR)catch_block;
    RtlUnwindEx(frame, inner_on, &consolidate, NULL, ctx, dispatch->HistoryTable);
    return ExceptionContinueSearch;
}

/* a frame between the try and the raise, unwound by the inner catch: the
 * rethrow's dispatch never comes back to it */
EXCEPTION_DISPOSITION mid_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, DISPATCHER_CONTEXT *dispatch)
{
    (void)frame; (void)ctx; (void)dispatch;
    if (!(rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)) && rec->ExceptionCode == AGAIN)
        seen_again = 1;
    return ExceptionContinueSearch;
}

EXCEPTION_DISPOSITION outer_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, DISPATCHER_CONTEXT *dispatch)
{
    if ((rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)) || rec->ExceptionCode != AGAIN)
        return ExceptionContinueSearch;
    RtlUnwindEx(frame, outer_on, rec, (void *)1, ctx, dispatch->HistoryTable);
    return ExceptionContinueSearch;
}

static void raise_it(void)
{
    RaiseException(FIRST, 0, 0, NULL);
}

static void mid(void)
{
    mid_guarded(raise_it);
}

static void inner(void)
{
    inner_guarded(mid);
    printf("fh4 rethrow: the inner catch returned (rethrown %d)\n", rethrown);
}

int main(void)
{
    if (outer_guarded(inner) != 1 || !rethrown)
    {
        printf("fh4 rethrow not caught\n");
        return 1;
    }
    if (seen_again)
    {
        printf("fh4 rethrow went back through the frames the catch unwound\n");
        return 1;
    }
    printf("fh4 rethrow caught\n");
    return 0;
}
