/* A catch(...) under __CxxFrameHandler4 (vcruntime140_1's, as MSVC's x64
 * C++ has used by default since VS2019) whose try block starts at state 1,
 * not 0: the handler must find the frame's state from its ip map. MinGW
 * emits no FH4 data, so the function and its catch funclet are written here
 * in assembly with FH4 tables by hand; the unwind info names a thunk that
 * jumps to vcruntime140_1's handler, found at run time. The exception is a
 * plain RaiseException, which catch(...) takes as well.
 * Prints "fh4 caught" and exits 0, or "fh4 not caught" (exit 1); an
 * exception the handler misses ends the process instead. */
#include <windows.h>
#include <stdio.h>

void *fh4_handler_ptr;
int fh4_catch(void (*fn)(void));

__asm__(
    ".text\n"
    /* the language-specific handler: a tail jump to vcruntime140_1's */
    ".globl fh4_thunk\n"
    "fh4_thunk:\n"
    "    jmp *fh4_handler_ptr(%rip)\n"

    /* int fh4_catch(void (*fn)(void)): state -1, then 0, then the try
     * block (state 1) around the call; 1 when its catch(...) ran */
    ".globl fh4_catch\n"
    ".def fh4_catch; .scl 2; .type 32; .endef\n"
    ".seh_proc fh4_catch\n"
    "fh4_catch:\n"
    "    pushq %rbp\n"
    "    .seh_pushreg %rbp\n"
    "    subq $48, %rsp\n"
    "    .seh_stackalloc 48\n"
    "    .seh_endprologue\n"
    "    movq %rcx, 32(%rsp)\n"
    "    movl $0, 40(%rsp)\n"
    "fh4_s0:\n"
    "    nop\n"
    "fh4_s1:\n"
    "    callq *32(%rsp)\n"
    "    nop\n"
    "fh4_s1_end:\n"
    "    nop\n"
    "fh4_cont:\n"
    "    movl 40(%rsp), %eax\n"
    "    addq $48, %rsp\n"
    "    popq %rbp\n"
    "    retq\n"
    "    .seh_handler fh4_thunk, @except, @unwind\n"
    "    .seh_handlerdata\n"
    "    .rva fh4_descr\n"
    "    .text\n"
    ".seh_endproc\n"

    /* its catch(...) funclet: rdx the parent's frame (its rsp after the
     * prologue); sets the result and returns where the parent goes on */
    ".def fh4_funclet; .scl 3; .type 32; .endef\n"
    ".seh_proc fh4_funclet\n"
    "fh4_funclet:\n"
    "    movq %rdx, 16(%rsp)\n"
    "    pushq %rbp\n"
    "    .seh_pushreg %rbp\n"
    "    subq $32, %rsp\n"
    "    .seh_stackalloc 32\n"
    "    .seh_endprologue\n"
    "    movl $1, 40(%rdx)\n"
    "    leaq fh4_cont(%rip), %rax\n"
    "    addq $32, %rsp\n"
    "    popq %rbp\n"
    "    retq\n"
    ".seh_endproc\n"

    /* FH4 tables; small numbers are one byte, the value shifted left 1 */
    ".section .rdata,\"dr\"\n"
    "fh4_descr:\n"
    "    .byte 0x18\n"                      /* unwind map, try block map */
    "    .rva fh4_unwind\n"
    "    .rva fh4_try\n"
    "    .rva fh4_ip\n"
    "fh4_unwind:\n"
    "    .byte 3 << 1\n"                    /* three states, no handlers: */
    "    .byte (1 << 2) << 1\n"             /* 0: before the map (none) */
    "    .byte (1 << 2) << 1\n"             /* 1: back to 0 */
    "    .byte (2 << 2) << 1\n"             /* 2 (the catch): back to 0 */
    "fh4_try:\n"
    "    .byte 1 << 1\n"                    /* one try block: */
    "    .byte 1 << 1, 1 << 1, 2 << 1\n"    /* states 1..1, catch at 2 */
    "    .rva fh4_catches\n"
    "fh4_catches:\n"
    "    .byte 1 << 1\n"                    /* one catch block: */
    "    .byte 0\n"                         /* no type: catch(...) */
    "    .rva fh4_funclet\n"
    "fh4_ip:\n"
    "    .byte 4 << 1\n"                    /* (ip delta, state + 1): */
    "    .byte 0, 0\n"
    "    .byte (fh4_s0 - fh4_catch) << 1, 1 << 1\n"
    "    .byte (fh4_s1 - fh4_s0) << 1, 2 << 1\n"
    "    .byte (fh4_s1_end - fh4_s1) << 1, 0\n"
    ".text\n"
);

static void raise_it(void)
{
    RaiseException(0x2000f4f4, 0, 0, NULL);
}

int main(void)
{
    HMODULE vcr = LoadLibraryA("vcruntime140_1.dll");

    fh4_handler_ptr = vcr ? (void *)GetProcAddress(vcr, "__CxxFrameHandler4") : NULL;
    if (!fh4_handler_ptr)
    {
        printf("fh4: no vcruntime140_1!__CxxFrameHandler4\n");
        return 2;
    }
    if (fh4_catch(raise_it) != 1)
    {
        printf("fh4 not caught\n");
        return 1;
    }
    printf("fh4 caught\n");
    return 0;
}
