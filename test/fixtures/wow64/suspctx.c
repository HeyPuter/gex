/* Another thread's context, with its x87/SSE state, while the thread runs
 * guest code (32-bit Wine's suspend path published only the integer, control and
 * segment registers, so the floating-point state came back as zeros). A
 * thread spins in a loop that touches no FP register, with xmm0-xmm7 and
 * MXCSR holding known values; another suspends it again and again and reads
 * its context with CONTEXT_FLOATING_POINT | CONTEXT_EXTENDED_REGISTERS.
 * Every context must show those values. Last, a SetThreadContext changes
 * xmm7 in a suspended thread, and the thread must see the new value. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;

static void check(int ok, const char *what, double got)
{
    if (ok) printf("ok %s (%.0f)\n", what, got);
    else { printf("FAIL %s (%.0f)\n", what, got); failures++; }
}

/* spin(regs, stop, count): xmm0-xmm7 = regs[0..7], MXCSR = regs[16]'s
 * first dword, then count++ until *stop, then regs[8..15] = xmm0-xmm7. */
void __cdecl spin(unsigned char *regs, volatile LONG *stop, volatile LONG *count);
__asm__(
    ".globl _spin\n"
    "_spin:\n"
    "  push %ebx\n  push %esi\n  push %edi\n"
    "  mov 16(%esp), %ebx\n  mov 20(%esp), %esi\n  mov 24(%esp), %edi\n"
    "  movdqu 0(%ebx), %xmm0\n  movdqu 16(%ebx), %xmm1\n  movdqu 32(%ebx), %xmm2\n"
    "  movdqu 48(%ebx), %xmm3\n  movdqu 64(%ebx), %xmm4\n  movdqu 80(%ebx), %xmm5\n"
    "  movdqu 96(%ebx), %xmm6\n  movdqu 112(%ebx), %xmm7\n"
    "  ldmxcsr 256(%ebx)\n"
    "1:\n"
    "  lock incl (%edi)\n"
    "  mov (%esi), %eax\n  test %eax, %eax\n  jz 1b\n"
    "  movdqu %xmm0, 128(%ebx)\n  movdqu %xmm1, 144(%ebx)\n  movdqu %xmm2, 160(%ebx)\n"
    "  movdqu %xmm3, 176(%ebx)\n  movdqu %xmm4, 192(%ebx)\n  movdqu %xmm5, 208(%ebx)\n"
    "  movdqu %xmm6, 224(%ebx)\n  movdqu %xmm7, 240(%ebx)\n"
    "  stmxcsr 260(%ebx)\n"
    "  mov $0x1f80, %eax\n  mov %eax, 264(%ebx)\n  ldmxcsr 264(%ebx)\n"
    "  pop %edi\n  pop %esi\n  pop %ebx\n  ret\n");

static unsigned char regs[272];
static volatile LONG stop, count;

static DWORD WINAPI spinner(void *arg)
{
    (void)arg;
    spin(regs, &stop, &count);
    return 0;
}

int main(void)
{
    HANDLE t;
    CONTEXT c;
    unsigned char want7[16];
    int i, fp = 0, xr = 0, bad = 0, taken = 0, set = 0;
    const DWORD mxcsr = 0x9fc0; /* FTZ, DAZ, every exception masked */

    for (i = 0; i < 128; i++) regs[i] = (unsigned char)(i * 37 + 11);
    memcpy(regs + 256, &mxcsr, 4);
    t = CreateThread(NULL, 0, spinner, NULL, 0, NULL);
    while (count < 1000) Sleep(1);
    for (i = 0; i < 2000; i++)
    {
        memset(&c, 0, sizeof c);
        c.ContextFlags = CONTEXT_FULL | CONTEXT_FLOATING_POINT | CONTEXT_EXTENDED_REGISTERS;
        if (SuspendThread(t) == (DWORD)-1) break;
        if (GetThreadContext(t, &c))
        {
            const BYTE *fx = c.ExtendedRegisters; /* FXSAVE: MXCSR at 24, xmm0 at 160 */
            DWORD mx;
            memcpy(&mx, fx + 24, 4);
            taken++;
            if (c.ContextFlags & CONTEXT_FLOATING_POINT & ~CONTEXT_i386) fp++;
            if (c.ContextFlags & CONTEXT_EXTENDED_REGISTERS & ~CONTEXT_i386)
            {
                xr++;
                if (memcmp(fx + 160, regs, 128) || mx != mxcsr)
                {
                    if (bad < 4) printf("bad %d: mxcsr %#lx\n", i, (unsigned long)mx);
                    bad++;
                }
            }
        }
        ResumeThread(t);
        if (i % 16 == 0) Sleep(0);
    }
    /* xmm7 changed in the suspended thread */
    for (i = 0; i < 16; i++) want7[i] = (unsigned char)(0xa5 ^ i);
    memset(&c, 0, sizeof c);
    c.ContextFlags = CONTEXT_FULL | CONTEXT_EXTENDED_REGISTERS;
    if (SuspendThread(t) != (DWORD)-1 && GetThreadContext(t, &c))
    {
        memcpy(c.ExtendedRegisters + 160 + 7 * 16, want7, 16);
        c.ContextFlags = CONTEXT_EXTENDED_REGISTERS;
        set = SetThreadContext(t, &c);
        ResumeThread(t);
    }
    InterlockedExchange(&stop, 1);
    WaitForSingleObject(t, INFINITE);
    check(taken == 2000, "contexts taken", taken);
    check(fp == taken && xr == taken, "contexts with the floating-point state", xr);
    check(bad == 0, "of them showing other xmm0-xmm7 or MXCSR than the thread's", bad);
    check(set && !memcmp(regs + 128, regs, 112), "xmm0-xmm6 unchanged", count);
    check(set && !memcmp(regs + 240, want7, 16), "xmm7 as SetThreadContext set it", set);
    check(!memcmp(regs + 260, &mxcsr, 4), "MXCSR unchanged", 0);
    printf("suspctx %d failures\n", failures);
    return failures != 0;
}
