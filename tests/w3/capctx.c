/* RtlCaptureContext on W3 (tests/w3/capctx.sh). Windows' and signal_i386.c's
 * i386 RtlCaptureContext write ContextFlags (CONTEXT_FULL), the segment,
 * integer and control registers, all below offset 0xcc, and take Ebp, Eip
 * and Esp from the caller's EBP frame. Programs rely on both: Wine Mono's
 * release_call_wrapper (mono/metadata/cominterop.c) reserves only 0xcc bytes
 * for the context and resumes it with NtContinue to return from itself.
 *   - fields: a CONTEXT at the start of a buffer filled with 0xa5 has
 *     ContextFlags 0x10007, Ebp/Eip/Esp of the caller's frame, and nothing
 *     else written: Dr0-Dr7, FloatSave and everything from 0xcc on intact;
 *   - resume: an assembly function in the style of release_call_wrapper (a
 *     0xcc-byte context just below its frame) captures, sets Eax to its
 *     argument + 1 and Esp past its argument, and NtContinues: it returns
 *     arg + 1 to its caller, its frame and argument not overwritten.
 * Prints "capctx ok". */
#include <windows.h>
#include <winternl.h>
#include <stdio.h>
#include <string.h>
#include <stddef.h>

NTSTATUS WINAPI NtContinue(CONTEXT *, BOOLEAN);

static int bad;
#define FAIL(...) do { bad++; printf(__VA_ARGS__); } while (0)

static unsigned char buf[0x400];

static void __attribute__((noinline)) fields(void)
{
    CONTEXT *c = (CONTEXT *)buf;
    void **fp = __builtin_frame_address(0);
    size_t i;

    memset(buf, 0xa5, sizeof buf);
    RtlCaptureContext(c);
    if (c->ContextFlags != 0x10007) FAIL("ContextFlags %lx\n", c->ContextFlags);
    if (c->Ebp != (DWORD)fp[0]) FAIL("Ebp %lx, caller's frame %p\n", c->Ebp, fp[0]);
    if (c->Eip != (DWORD)fp[1]) FAIL("Eip %lx, return address %p\n", c->Eip, fp[1]);
    if (c->Esp != (DWORD)(fp + 2)) FAIL("Esp %lx, caller's stack %p\n", c->Esp, fp + 2);
    if (c->SegCs != 0x23 || c->SegSs != 0x2b) FAIL("SegCs %lx SegSs %lx\n", c->SegCs, c->SegSs);
    for (i = offsetof(CONTEXT, Dr0); i < offsetof(CONTEXT, SegGs); i++)
        if (buf[i] != 0xa5) { FAIL("written at %#x (debug/FloatSave)\n", (unsigned)i); break; }
    for (i = offsetof(CONTEXT, ExtendedRegisters); i < sizeof buf; i++)
        if (buf[i] != 0xa5) { FAIL("written at %#x (past SegSs)\n", (unsigned)i); break; }
}

/* int __stdcall resume(int x): release_call_wrapper's frame, without the
 * exception frame: returns x + 1 through NtContinue. */
int __stdcall resume(int x);
__asm__(".text\n\t"
        ".globl _resume@4\n"
        "_resume@4:\n\t"
        "pushl %ebp\n\t"
        "movl %esp,%ebp\n\t"
        "subl $0xcc,%esp\n\t"
        "pushl %esp\n\t"
        "call _RtlCaptureContext@4\n\t"
        "movl 8(%ebp),%eax\n\t"
        "incl %eax\n\t"
        "movl %eax,0xb0(%esp)\n\t"   /* context->Eax */
        "leal 12(%ebp),%eax\n\t"
        "movl %eax,0xc4(%esp)\n\t"   /* context->Esp: past the argument */
        "movl %esp,%eax\n\t"
        "pushl $0\n\t"
        "pushl %eax\n\t"
        "call _NtContinue@8\n\t"
        "movl $-1,%eax\n\t"          /* not reached */
        "leave\n\t"
        "ret $4\n\t");

int main(void)
{
    int r, i;

    fields();
    for (i = 0; i < 3; i++)
        if ((r = resume(41 + i)) != 42 + i) FAIL("resume(%d) returned %d\n", 41 + i, r);
    if (!bad) printf("capctx ok\n");
    fflush(stdout);
    return bad != 0;
}
