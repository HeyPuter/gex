/* A guest function called from native Wine code that catches an exception
 * its callee raised in its own frame, then returns (test/wow64/strayret).
 *
 * ntdll's qsort (native wasm in 32-bit Wine) calls cmp through the function table
 * (gexrt `__gexw_guest_call`), with native Wine's return pad, 0x3ffff1, as
 * the return address. cmp has a function-tier unit; it registers an SEH
 * frame and calls `fault`, generated code no unit has (a call-out, run on
 * tier-1), which calls RaiseException. ntdll dispatches the exception to
 * cmp's handler, which resumes cmp at cmp_resume in cmp's own frame (as a
 * compiler's __except block does), not at the call's return. cmp then
 * returns to the pad while the call-out to `fault` is still open: its unit
 * body is waiting for a return that never comes. Windows runs this; gex
 * trapped with "breakpoint" at 0x3ffff1 (explorer in the
 * Paint site).
 *
 * Prints "strayret ok N" (N the exceptions handled) when the array is sorted and
 * every comparison was caught, with the registers cmp_resume left intact. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

typedef void (__cdecl *qsort_fn)(void *, size_t, size_t, int (__cdecl *)(const void *, const void *));

#define STRAYRET_CODE 0xe0000001u
unsigned char *fault_code;    /* RaiseException(STRAYRET_CODE, 0, 0, NULL); ret */
DWORD saved_esp, saved_ebp;
volatile LONG caught;
extern char cmp_resume[];
int __cdecl cmp(const void *, const void *);
/* The thread case: a thread's start routine (called by native Wine's
 * BaseThreadInitThunk, also returning to the pad) with the same handler,
 * resuming in its own frame, then (its argument nonzero) sleeping that many
 * ms before it returns 0x55 (1000 when a register is lost). */
DWORD t_saved_esp, t_saved_ebp;
extern char tproc_resume[];
DWORD WINAPI tproc(void *);
void *sleep_fn;
/* The outer-handler thread case: the start routine registers the handler
 * and calls native Wine (ntdll's qsort), whose guest callback raises; the
 * handler resumes the start routine past the qsort call, leaving qsort's
 * native frames, and it then returns to its native caller's pad. */
DWORD t2_saved_esp, t2_saved_ebp;
extern char tproc2_resume[];
DWORD WINAPI tproc2(void *);
void *qsort_fn_p;
int t2_v[4] = { 3, 1, 2, 0 };
int restore_regs;
int __cdecl cmpraise(const void *a, const void *b) { (void)a; (void)b; ((void (*)(void))fault_code)(); return 0; }

EXCEPTION_DISPOSITION __cdecl tproc2_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *disp)
{
    (void)frame; (void)disp;
    if (rec->ExceptionCode != STRAYRET_CODE || (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)))
        return ExceptionContinueSearch;
    caught++;
    ctx->Esp = t2_saved_esp;
    ctx->Ebp = t2_saved_ebp;
    ctx->Eip = (DWORD)tproc2_resume;
    /* "rtl" (WoW64): qsort is guest code there, and the raise's ebx, esi
     * and edi are its own; the handler gives back the start routine's, as
     * a compiler's __except restores them from its frame. */
    if (restore_regs) { ctx->Ebx = 0x1234; ctx->Esi = 0x5678; ctx->Edi = 0x9abc; }
    return ExceptionContinueExecution;
}

EXCEPTION_DISPOSITION __cdecl tproc_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *disp)
{
    (void)frame; (void)disp;
    if (rec->ExceptionCode != STRAYRET_CODE || (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)))
        return ExceptionContinueSearch;
    caught++;
    ctx->Esp = t_saved_esp;
    ctx->Ebp = t_saved_ebp;
    ctx->Eip = (DWORD)tproc_resume;
    return ExceptionContinueExecution;
}

EXCEPTION_DISPOSITION __cdecl strayret_handler(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *disp)
{
    (void)frame; (void)disp;
    if (rec->ExceptionCode != STRAYRET_CODE || (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)))
        return ExceptionContinueSearch;
    caught++;
    ctx->Esp = saved_esp;
    ctx->Ebp = saved_ebp;
    ctx->Eip = (DWORD)cmp_resume;
    return ExceptionContinueExecution;
}

/* cmp: an SEH record on its stack, the faulting call, and the resume point
 * the handler sends it to. ebx, esi and edi carry values across the fault
 * that cmp_resume checks (eax gets 1000 when one is lost). */
__asm__(
    ".text\n"
    ".globl _cmp\n"
    "_cmp:\n"
    "  push %ebp\n"
    "  mov %esp, %ebp\n"
    "  push %ebx\n"
    "  push %esi\n"
    "  push %edi\n"
    "  push $_strayret_handler\n"
    "  push %fs:0\n"
    "  mov %esp, %fs:0\n"
    "  mov %esp, _saved_esp\n"
    "  mov %ebp, _saved_ebp\n"
    "  mov $0x1234, %ebx\n"
    "  mov $0x5678, %esi\n"
    "  mov $0x9abc, %edi\n"
    "  call *_fault_code\n"
    "  mov $2000, %eax\n"          /* not reached: the fault never returns */
    "  jmp 2f\n"
    ".globl _cmp_resume\n"
    "_cmp_resume:\n"
    "  mov 8(%ebp), %eax\n"
    "  mov (%eax), %eax\n"
    "  mov 12(%ebp), %ecx\n"
    "  sub (%ecx), %eax\n"
    "  cmp $0x1234, %ebx\n"
    "  jne 1f\n"
    "  cmp $0x5678, %esi\n"
    "  jne 1f\n"
    "  cmp $0x9abc, %edi\n"
    "  je 2f\n"
    "1:\n"
    "  mov $1000, %eax\n"
    "2:\n"
    "  mov (%esp), %ecx\n"
    "  mov %ecx, %fs:0\n"
    "  add $8, %esp\n"
    "  pop %edi\n"
    "  pop %esi\n"
    "  pop %ebx\n"
    "  pop %ebp\n"
    "  ret\n");

__asm__(
    ".text\n"
    ".globl _tproc@4\n"
    "_tproc@4:\n"
    "  push %ebp\n"
    "  mov %esp, %ebp\n"
    "  push %ebx\n"
    "  push %esi\n"
    "  push %edi\n"
    "  push $_tproc_handler\n"
    "  push %fs:0\n"
    "  mov %esp, %fs:0\n"
    "  mov %esp, _t_saved_esp\n"
    "  mov %ebp, _t_saved_ebp\n"
    "  mov $0x1234, %ebx\n"
    "  mov $0x5678, %esi\n"
    "  mov $0x9abc, %edi\n"
    "  call *_fault_code\n"
    "  mov $2000, %eax\n"
    "  jmp 2f\n"
    ".globl _tproc_resume\n"
    "_tproc_resume:\n"
    "  mov 8(%ebp), %eax\n"
    "  test %eax, %eax\n"
    "  jz 3f\n"
    "  push %eax\n"
    "  call *_sleep_fn\n"
    "3:\n"
    "  mov $0x55, %eax\n"
    "  cmp $0x1234, %ebx\n"
    "  jne 1f\n"
    "  cmp $0x5678, %esi\n"
    "  jne 1f\n"
    "  cmp $0x9abc, %edi\n"
    "  je 2f\n"
    "1:\n"
    "  mov $1000, %eax\n"
    "2:\n"
    "  mov (%esp), %ecx\n"
    "  mov %ecx, %fs:0\n"
    "  add $8, %esp\n"
    "  pop %edi\n"
    "  pop %esi\n"
    "  pop %ebx\n"
    "  pop %ebp\n"
    "  ret $4\n");

__asm__(
    ".text\n"
    ".globl _tproc2@4\n"
    "_tproc2@4:\n"
    "  push %ebp\n"
    "  mov %esp, %ebp\n"
    "  push %ebx\n"
    "  push %esi\n"
    "  push %edi\n"
    "  push $_tproc2_handler\n"
    "  push %fs:0\n"
    "  mov %esp, %fs:0\n"
    "  mov %esp, _t2_saved_esp\n"
    "  mov %ebp, _t2_saved_ebp\n"
    "  mov $0x1234, %ebx\n"
    "  mov $0x5678, %esi\n"
    "  mov $0x9abc, %edi\n"
    "  push $_cmpraise\n"
    "  push $4\n"
    "  push $4\n"
    "  push $_t2_v\n"
    "  call *_qsort_fn_p\n"
    "  add $16, %esp\n"
    "  mov $2000, %eax\n"
    "  jmp 2f\n"
    ".globl _tproc2_resume\n"
    "_tproc2_resume:\n"
    "  mov 8(%ebp), %eax\n"
    "  test %eax, %eax\n"
    "  jz 3f\n"
    "  push %eax\n"
    "  call *_sleep_fn\n"
    "3:\n"
    "  mov $0x55, %eax\n"
    "  cmp $0x1234, %ebx\n"
    "  jne 1f\n"
    "  cmp $0x5678, %esi\n"
    "  jne 1f\n"
    "  cmp $0x9abc, %edi\n"
    "  je 2f\n"
    "1:\n"
    "  mov $1000, %eax\n"
    "2:\n"
    "  mov (%esp), %ecx\n"
    "  mov %ecx, %fs:0\n"
    "  add $8, %esp\n"
    "  pop %edi\n"
    "  pop %esi\n"
    "  pop %ebx\n"
    "  pop %ebp\n"
    "  ret $4\n");

/* The thread case with `ms` of sleep after the resume: its exit code. */
static DWORD thread_case(LPTHREAD_START_ROUTINE f, DWORD ms)
{
    DWORD code = 0;
    HANDLE h = CreateThread(NULL, 0, f, (void *)(ULONG_PTR)ms, 0, NULL);
    if (!h) return 0;
    if (WaitForSingleObject(h, 30000) != WAIT_OBJECT_0) code = 3000;
    else GetExitCodeThread(h, &code);
    CloseHandle(h);
    return code;
}

/* "rtl": the generated code raises through ntdll's RtlRaiseException
 * itself (a static record), whose context is the caller's registers as
 * they are: kernel32's RaiseException, a PE under WoW64, keeps its own in
 * ebx, esi and edi when it raises, so a handler resuming with that context
 * sees them, not cmp's. */
static EXCEPTION_RECORD strayret_rec = { STRAYRET_CODE };

int main(int argc, char **argv)
{
    int v[] = { 7, 3, 9, 1, 8, 2, 6, 5, 4, 0 };
    enum { N = sizeof v / sizeof v[0] };
    qsort_fn qs = (qsort_fn)GetProcAddress(GetModuleHandleA("ntdll.dll"), "qsort");
    int i;
    if (!qs) { printf("strayret: no ntdll qsort\n"); return 1; }
    /* push 0; push 0; push 0; push STRAYRET_CODE; mov eax, RaiseException; call eax; ret */
    static const unsigned char code[] = { 0x6a, 0, 0x6a, 0, 0x6a, 0, 0x68, 0x01, 0, 0, 0xe0, 0xb8, 0, 0, 0, 0, 0xff, 0xd0, 0xc3 };
    void *raise = (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "RaiseException");
    fault_code = VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    memcpy(fault_code, code, sizeof code);
    memcpy(fault_code + 12, &raise, 4);
    if (argc > 1 && !strcmp(argv[1], "rtl")) {
        /* push &strayret_rec; mov eax, RtlRaiseException; call eax; ret */
        static const unsigned char rtl[] = { 0x68, 0, 0, 0, 0, 0xb8, 0, 0, 0, 0, 0xff, 0xd0, 0xc3 };
        void *rr = (void *)GetProcAddress(GetModuleHandleA("ntdll.dll"), "RtlRaiseException");
        void *rec = &strayret_rec;
        memcpy(fault_code, rtl, sizeof rtl);
        memcpy(fault_code + 1, &rec, 4);
        memcpy(fault_code + 6, &rr, 4);
        restore_regs = 1;
    }
    qs(v, N, sizeof v[0], cmp);
    for (i = 0; i < N; i++)
        if (v[i] != i) {
            printf("strayret: not sorted (a lost register gives 1000s), %ld exceptions caught:", caught);
            for (i = 0; i < N; i++) printf(" %d", v[i]);
            printf("\n");
            return 1;
        }
    if (caught < N - 1) { printf("strayret: %ld exceptions caught, want %d or more\n", caught, N - 1); return 1; }
    sleep_fn = (void *)GetProcAddress(GetModuleHandleA("kernel32.dll"), "Sleep");
    qsort_fn_p = (void *)qs;
    for (i = 0; i < 4; i++) {
        DWORD ms = (i & 1) ? 50 : 0, code = thread_case(i < 2 ? tproc : tproc2, ms);
        if (code != 0x55) { printf("strayret: thread case %d (sleep %lu): exit code %lu, want 85\n", i, ms, code); return 1; }
    }
    printf("strayret ok %ld\n", caught);
    return 0;
}
