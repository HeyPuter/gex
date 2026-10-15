/* A guest function called from native Wine code that catches an exception
 * its callee raised in its own frame, then returns (tests/w3/strayret.sh).
 *
 * ntdll's qsort (native wasm in W3) calls cmp through the function table
 * (gexrt `__gexw_guest_call`), with native Wine's return pad, 0x3ffff1, as
 * the return address. cmp has a function-tier unit; it registers an SEH
 * frame and calls `fault`, generated code no unit has (a call-out, run on
 * tier-1), which calls RaiseException. ntdll dispatches the exception to
 * cmp's handler, which resumes cmp at cmp_resume in cmp's own frame (as a
 * compiler's __except block does), not at the call's return. cmp then
 * returns to the pad while the call-out to `fault` is still open: its unit
 * body is waiting for a return that never comes. Windows runs this; gex
 * trapped with "breakpoint" at 0x3ffff1 (2026-10-14, explorer in the
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

int main(void)
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
    qs(v, N, sizeof v[0], cmp);
    for (i = 0; i < N; i++)
        if (v[i] != i) {
            printf("strayret: not sorted (a lost register gives 1000s), %ld exceptions caught:", caught);
            for (i = 0; i < N; i++) printf(" %d", v[i]);
            printf("\n");
            return 1;
        }
    if (caught < N - 1) { printf("strayret: %ld exceptions caught, want %d or more\n", caught, N - 1); return 1; }
    printf("strayret ok %ld\n", caught);
    return 0;
}
