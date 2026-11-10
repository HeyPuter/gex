/* A rethrow (`throw;`) from a catch block under __CxxFrameHandler4
 * (vcruntime140_1's, MSVC's x64 C++ since VS2019): the frames below the
 * catch were unwound when it was entered, and the rethrow's unwind goes on
 * from the frame that caught. FH4 keeps no state in a frame (the state to
 * unwind from comes from the ip map, or from the catch's own bookkeeping),
 * so a frame unwound twice runs its cleanup twice, the second time from
 * another frame's state.
 *
 * As in fh4catch.c the functions are written in assembly with FH4 tables
 * by hand (MinGW emits none): fh4_inner has one cleanup state around its
 * call; fh4_mid's catch(...) rethrows through _CxxThrowException(NULL,
 * NULL); fh4_outer's catch(...) takes the rethrown exception. The
 * exception is a plain RaiseException, which catch(...) takes as well.
 * Prints "fh4 rethrow: caught, cleanups N" and exits 0 when it was caught
 * and the cleanup ran once, else 1. */
#include <windows.h>
#include <stdio.h>

void *fh4_handler_ptr, *fh4_throw_ptr, *fh4_before_ptr;
int fh4_outer(void (*fn)(void));
void fh4_mid(void (*fn)(void));
static volatile int cleanups;

void fh4_cleanup(void *obj, ULONG64 frame)
{
    cleanups++;
}

__asm__(
    ".text\n"
    ".globl fh4_thunk\n"
    "fh4_thunk:\n"
    "    jmp *fh4_handler_ptr(%rip)\n"

    /* void fh4_inner(void (*fn)(void)): state 0 (cleanup: fh4_cleanup)
     * around the call */
    ".def fh4_inner; .scl 3; .type 32; .endef\n"
    ".seh_proc fh4_inner\n"
    "fh4_inner:\n"
    "    pushq %rbp\n"
    "    .seh_pushreg %rbp\n"
    "    subq $48, %rsp\n"
    "    .seh_stackalloc 48\n"
    "    .seh_endprologue\n"
    "    movq %rcx, 32(%rsp)\n"
    "fi_s0:\n"
    "    callq *32(%rsp)\n"
    "    nop\n"
    "fi_end:\n"
    "    addq $48, %rsp\n"
    "    popq %rbp\n"
    "    retq\n"
    "    .seh_handler fh4_thunk, @unwind\n"
    "    .seh_handlerdata\n"
    "    .rva fi_descr\n"
    "    .text\n"
    ".seh_endproc\n"

    /* void fh4_mid(void (*fn)(void)): a try block (state 1) around
     * fh4_inner(fn), its catch(...) a rethrow */
    ".globl fh4_mid\n"
    ".def fh4_mid; .scl 2; .type 32; .endef\n"
    ".seh_proc fh4_mid\n"
    "fh4_mid:\n"
    "    pushq %rbp\n"
    "    .seh_pushreg %rbp\n"
    "    subq $48, %rsp\n"
    "    .seh_stackalloc 48\n"
    "    .seh_endprologue\n"
    "    movq %rcx, 32(%rsp)\n"
    "fm_s0:\n"
    "    nop\n"
    "fm_s1:\n"
    "    callq *fh4_before_ptr(%rip)\n"
    "    movq 32(%rsp), %rcx\n"
    "    callq fh4_inner\n"
    "    nop\n"
    "fm_s1_end:\n"
    "    nop\n"
    "fm_cont:\n"
    "    addq $48, %rsp\n"
    "    popq %rbp\n"
    "    retq\n"
    "    .seh_handler fh4_thunk, @except, @unwind\n"
    "    .seh_handlerdata\n"
    "    .rva fm_descr\n"
    "    .text\n"
    ".seh_endproc\n"

    /* its catch(...) funclet: throw; */
    ".def fm_funclet; .scl 3; .type 32; .endef\n"
    ".seh_proc fm_funclet\n"
    "fm_funclet:\n"
    "    movq %rdx, 16(%rsp)\n"
    "    pushq %rbp\n"
    "    .seh_pushreg %rbp\n"
    "    subq $32, %rsp\n"
    "    .seh_stackalloc 32\n"
    "    .seh_endprologue\n"
    "    xorl %ecx, %ecx\n"
    "    xorl %edx, %edx\n"
    "    callq *fh4_throw_ptr(%rip)\n"
    "    int3\n"
    "fmf_end:\n"
    "    leaq fm_cont(%rip), %rax\n"
    "    addq $32, %rsp\n"
    "    popq %rbp\n"
    "    retq\n"
    "    .seh_handler fh4_thunk, @except, @unwind\n"
    "    .seh_handlerdata\n"
    "    .rva fmf_descr\n"
    "    .text\n"
    ".seh_endproc\n"

    /* int fh4_outer(void (*fn)(void)): fh4catch.c's fh4_catch, a try block
     * (state 1) around the call; 1 when its catch(...) ran */
    ".globl fh4_outer\n"
    ".def fh4_outer; .scl 2; .type 32; .endef\n"
    ".seh_proc fh4_outer\n"
    "fh4_outer:\n"
    "    pushq %rbp\n"
    "    .seh_pushreg %rbp\n"
    "    subq $48, %rsp\n"
    "    .seh_stackalloc 48\n"
    "    .seh_endprologue\n"
    "    movq %rcx, 32(%rsp)\n"
    "    movl $0, 40(%rsp)\n"
    "fo_s0:\n"
    "    nop\n"
    "fo_s1:\n"
    "    callq *32(%rsp)\n"
    "    nop\n"
    "fo_s1_end:\n"
    "    nop\n"
    "fo_cont:\n"
    "    movl 40(%rsp), %eax\n"
    "    addq $48, %rsp\n"
    "    popq %rbp\n"
    "    retq\n"
    "    .seh_handler fh4_thunk, @except, @unwind\n"
    "    .seh_handlerdata\n"
    "    .rva fo_descr\n"
    "    .text\n"
    ".seh_endproc\n"

    ".def fo_funclet; .scl 3; .type 32; .endef\n"
    ".seh_proc fo_funclet\n"
    "fo_funclet:\n"
    "    movq %rdx, 16(%rsp)\n"
    "    pushq %rbp\n"
    "    .seh_pushreg %rbp\n"
    "    subq $32, %rsp\n"
    "    .seh_stackalloc 32\n"
    "    .seh_endprologue\n"
    "    movl $1, 40(%rdx)\n"
    "    leaq fo_cont(%rip), %rax\n"
    "    addq $32, %rsp\n"
    "    popq %rbp\n"
    "    retq\n"
    ".seh_endproc\n"

    /* FH4 tables; small numbers are one byte, the value shifted left 1 */
    ".section .rdata,\"dr\"\n"
    "fi_descr:\n"
    "    .byte 0x08\n"                      /* unwind map */
    "    .rva fi_unwind\n"
    "    .rva fi_ip\n"
    "fi_unwind:\n"
    "    .byte 1 << 1\n"                    /* one state: */
    "    .byte ((1 << 2) | 3) << 1\n"       /* 0: back to none, a frame cleanup */
    "    .rva fh4_cleanup\n"
    "fi_ip:\n"
    "    .byte 3 << 1\n"                    /* (ip delta, state + 1): */
    "    .byte 0, 0\n"
    "    .byte (fi_s0 - fh4_inner) << 1, 1 << 1\n"
    "    .byte (fi_end - fi_s0) << 1, 0\n"

    "fm_descr:\n"
    "    .byte 0x18\n"                      /* unwind map, try block map */
    "    .rva fm_unwind\n"
    "    .rva fm_try\n"
    "    .rva fm_ip\n"
    "fm_unwind:\n"
    "    .byte 3 << 1\n"
    "    .byte (1 << 2) << 1\n"
    "    .byte (1 << 2) << 1\n"
    "    .byte (2 << 2) << 1\n"
    "fm_try:\n"
    "    .byte 1 << 1\n"
    "    .byte 1 << 1, 1 << 1, 2 << 1\n"
    "    .rva fm_catches\n"
    "fm_catches:\n"
    "    .byte 1 << 1\n"
    "    .byte 0\n"
    "    .rva fm_funclet\n"
    "fm_ip:\n"
    "    .byte 4 << 1\n"
    "    .byte 0, 0\n"
    "    .byte (fm_s0 - fh4_mid) << 1, 1 << 1\n"
    "    .byte (fm_s1 - fm_s0) << 1, 2 << 1\n"
    "    .byte (fm_s1_end - fm_s1) << 1, 0\n"

    /* the funclet's own: a catch block (the parent's frame at 56, where
     * its prologue left RDX), no states */
    "fmf_descr:\n"
    "    .byte 0x01\n"
    "    .rva fmf_ip\n"
    "    .byte 56 << 1\n"
    "fmf_ip:\n"
    "    .byte 2 << 1\n"
    "    .byte 0, 0\n"
    "    .byte (fmf_end - fm_funclet) << 1, 0\n"

    "fo_descr:\n"
    "    .byte 0x18\n"
    "    .rva fo_unwind\n"
    "    .rva fo_try\n"
    "    .rva fo_ip\n"
    "fo_unwind:\n"
    "    .byte 3 << 1\n"
    "    .byte (1 << 2) << 1\n"
    "    .byte (1 << 2) << 1\n"
    "    .byte (2 << 2) << 1\n"
    "fo_try:\n"
    "    .byte 1 << 1\n"
    "    .byte 1 << 1, 1 << 1, 2 << 1\n"
    "    .rva fo_catches\n"
    "fo_catches:\n"
    "    .byte 1 << 1\n"
    "    .byte 0\n"
    "    .rva fo_funclet\n"
    "fo_ip:\n"
    "    .byte 4 << 1\n"
    "    .byte 0, 0\n"
    "    .byte (fo_s0 - fh4_outer) << 1, 1 << 1\n"
    "    .byte (fo_s1 - fo_s0) << 1, 2 << 1\n"
    "    .byte (fo_s1_end - fo_s1) << 1, 0\n"
    ".text\n"
);

static void raise_it(void)
{
    RaiseException(0x2000f4f4, 0, 0, NULL);
}

static void caught_before(void)
{
    fh4_outer(raise_it);
}

static void run_mid(void)
{
    fh4_mid(raise_it);
}

int main(void)
{
    HMODULE vcr = LoadLibraryA("vcruntime140_1.dll"), vc = LoadLibraryA("vcruntime140.dll");
    int caught;

    fh4_handler_ptr = vcr ? (void *)GetProcAddress(vcr, "__CxxFrameHandler4") : NULL;
    fh4_throw_ptr = vc ? (void *)GetProcAddress(vc, "_CxxThrowException") : NULL;
    if (!fh4_handler_ptr || !fh4_throw_ptr)
    {
        printf("fh4: no vcruntime140_1!__CxxFrameHandler4 or vcruntime140!_CxxThrowException\n");
        return 2;
    }
    /* first a catch that goes on in its frame, as a program's earlier
     * exceptions do */
    for (caught = 0; caught < 3; caught++) fh4_outer(raise_it);
    fh4_before_ptr = caught_before;
    caught = fh4_outer(run_mid);
    printf("fh4 rethrow: %s, cleanups %d\n", caught == 1 ? "caught" : "not caught", cleanups);
    return caught == 1 && cleanups == 1 ? 0 : 1;
}
