/* A guest exception handler that leaves native frames by jumping, then
 * calls native code before it returns to its native caller
 * (tests/w3/nativeleft.sh).
 *
 * ntdll's qsort (native wasm in W3, the outer native caller) calls
 * cmp_outer straight through the function table (it has a function-tier
 * unit). cmp_outer registers an SEH frame and calls qsort again (the inner
 * native caller) with cmp_mid, which registers a frame of its own (a
 * finally: it counts its unwind calls) and calls RaiseException. ntdll
 * dispatches to cmp_outer's handler, which does what an MSVC __except
 * handler does: RtlUnwind to its frame (cmp_mid's handler runs once, for
 * the unwind), then a jump to cmp_outer_resume with cmp_outer's stack. The
 * guest now runs in cmp_outer's frame, above the native frames of the
 * dispatch and of the inner qsort, which are still on the wasm stack with
 * no call-out between. cmp_outer_resume calls native code
 * (GetCurrentThreadId): gexrt sees the guest enter native code from above
 * the innermost native entry. Only the frames it left may go: the outer
 * qsort's are live, and cmp_outer then returns to it (with them gone, that
 * return is a breakpoint at the pad, 0x3ffff1). The same on new threads,
 * whose start routine native Wine calls.
 *
 * Prints "nativeleft main ok N" after the main thread's case and
 * "nativeleft ok N" after the threads' (N the exceptions handled so far)
 * when each array is sorted, every comparison's exception was handled and
 * each finally ran exactly once. */
#include <windows.h>
#include <stdio.h>

typedef void (__cdecl *qsort_fn)(void *, size_t, size_t, int (__cdecl *)(const void *, const void *));

#define NL_CODE 0xe0000002u
void *qsort_p, *raise_p, *unwind_p, *tid_p;
volatile LONG caught, finally_runs, outer_calls, mid_calls;
DWORD saved_esp, saved_ebp;
extern char cmp_outer_resume[];
int __cdecl cmp_outer(const void *, const void *);
int __cdecl cmp_mid(const void *, const void *);
DWORD WINAPI tproc(void *);

/* cmp_mid's frame: the finally of a frame the exception is unwound past. */
EXCEPTION_DISPOSITION __cdecl mid_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *disp)
{
    (void)frame; (void)ctx; (void)disp;
    if (rec->ExceptionCode == NL_CODE && (rec->ExceptionFlags & EXCEPTION_UNWINDING))
        InterlockedIncrement(&finally_runs);
    return ExceptionContinueSearch;
}

/* cmp_outer's handler, as MSVC's _except_handler4 runs an __except block:
 * unwind to the frame (RtlUnwind returns at the label after its call), put
 * back the frame's stack and jump to the block. One thread at a time runs
 * it (the saved stack is global). */
__asm__(
    ".text\n"
    ".globl _outer_handler\n"
    "_outer_handler:\n"
    "  mov 4(%esp), %eax\n"
    "  testl $6, 4(%eax)\n"            /* EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND */
    "  jnz 9f\n"
    "  cmpl $0xe0000002, (%eax)\n"
    "  jne 9f\n"
    "  lock incl _caught\n"
    "  mov 8(%esp), %ecx\n"
    "  push $0\n"
    "  push %eax\n"
    "  push $8f\n"
    "  push %ecx\n"
    "  call *_unwind_p\n"
    "8:\n"
    "  mov _saved_esp, %esp\n"
    "  mov _saved_ebp, %ebp\n"
    "  jmp _cmp_outer_resume\n"
    "9:\n"
    "  mov $1, %eax\n"                 /* ExceptionContinueSearch */
    "  ret\n");

/* cmp_outer: an SEH record, the inner qsort, and the __except block the
 * handler jumps to, which calls native code and then compares. */
__asm__(
    ".text\n"
    ".globl _cmp_outer\n"
    "_cmp_outer:\n"
    "  push %ebp\n"
    "  mov %esp, %ebp\n"
    "  push $_outer_handler\n"
    "  push %fs:0\n"
    "  mov %esp, %fs:0\n"
    "  mov %esp, _saved_esp\n"
    "  mov %ebp, _saved_ebp\n"
    "  lock incl _outer_calls\n"
    "  push $_cmp_mid\n"
    "  push $4\n"
    "  push $2\n"
    "  push $_mid_v\n"
    "  call *_qsort_p\n"
    "  add $16, %esp\n"
    "  mov $2000, %eax\n"              /* not reached: cmp_mid never returns */
    "  jmp 2f\n"
    ".globl _cmp_outer_resume\n"
    "_cmp_outer_resume:\n"
    "  call *_tid_p\n"
    "  test %eax, %eax\n"
    "  jz 1f\n"
    "  mov 8(%ebp), %eax\n"
    "  mov (%eax), %eax\n"
    "  mov 12(%ebp), %ecx\n"
    "  sub (%ecx), %eax\n"
    "  jmp 2f\n"
    "1:\n"
    "  mov $1000, %eax\n"
    "2:\n"
    "  mov (%esp), %ecx\n"
    "  mov %ecx, %fs:0\n"
    "  add $8, %esp\n"
    "  pop %ebp\n"
    "  ret\n");

int mid_v[2] = { 2, 1 };

/* cmp_mid: an SEH record (the finally) and the raise. */
__asm__(
    ".text\n"
    ".globl _cmp_mid\n"
    "_cmp_mid:\n"
    "  push %ebp\n"
    "  mov %esp, %ebp\n"
    "  push $_mid_handler\n"
    "  push %fs:0\n"
    "  mov %esp, %fs:0\n"
    "  lock incl _mid_calls\n"
    "  push $0\n"
    "  push $0\n"
    "  push $0\n"
    "  push $0xe0000002\n"
    "  call *_raise_p\n"
    "  mov (%esp), %ecx\n"             /* not reached */
    "  mov %ecx, %fs:0\n"
    "  add $8, %esp\n"
    "  pop %ebp\n"
    "  mov $3000, %eax\n"
    "  ret\n");

static int sort_checked(const char *what)
{
    int v[] = { 7, 3, 9, 1, 8, 2, 6, 5, 4, 0 };
    enum { N = sizeof v / sizeof v[0] };
    LONG c0 = caught, f0 = finally_runs, o0 = outer_calls, m0 = mid_calls;
    int i;
    ((qsort_fn)qsort_p)(v, N, sizeof v[0], cmp_outer);
    for (i = 0; i < N; i++)
        if (v[i] != i) {
            printf("nativeleft: %s: not sorted (1000: no thread id), %ld exceptions handled:", what, caught - c0);
            for (i = 0; i < N; i++) printf(" %d", v[i]);
            printf("\n");
            return 0;
        }
    if (outer_calls - o0 < N - 1 || caught - c0 != outer_calls - o0 || mid_calls - m0 != outer_calls - o0
        || finally_runs - f0 != caught - c0) {
        printf("nativeleft: %s: %ld comparisons, %ld raises, %ld handled, %ld finally runs (want each once per comparison)\n",
               what, outer_calls - o0, mid_calls - m0, caught - c0, finally_runs - f0);
        return 0;
    }
    return 1;
}

DWORD WINAPI tproc(void *arg)
{
    (void)arg;
    return sort_checked("thread") ? 0x55 : 1;
}

int main(void)
{
    HMODULE nt = GetModuleHandleA("ntdll.dll"), k32 = GetModuleHandleA("kernel32.dll");
    int i;
    qsort_p = (void *)GetProcAddress(nt, "qsort");
    unwind_p = (void *)GetProcAddress(nt, "RtlUnwind");
    raise_p = (void *)GetProcAddress(k32, "RaiseException");
    tid_p = (void *)GetProcAddress(k32, "GetCurrentThreadId");
    if (!qsort_p || !unwind_p || !raise_p || !tid_p) { printf("nativeleft: an export is missing\n"); return 1; }
    if (!sort_checked("main")) return 1;
    printf("nativeleft main ok %ld\n", caught);
    fflush(stdout);
    for (i = 0; i < 2; i++) {
        DWORD code = 0;
        HANDLE h = CreateThread(NULL, 0, tproc, NULL, 0, NULL);
        if (!h || WaitForSingleObject(h, 30000) != WAIT_OBJECT_0) { printf("nativeleft: thread %d did not end\n", i); return 1; }
        GetExitCodeThread(h, &code);
        CloseHandle(h);
        if (code != 0x55) { printf("nativeleft: thread %d: exit code %lu, want 85\n", i, code); return 1; }
    }
    printf("nativeleft ok %ld\n", caught);
    return 0;
}
