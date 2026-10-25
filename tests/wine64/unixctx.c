/* Another thread's context, taken while it is inside a Unix call (H, B1:
 * the Unix call's frame leaves the x87/SSE state in gex's context until
 * something reads it). A thread loops on gethostname -- ws2_32's Unix call
 * -- with xmm6-xmm15 holding known values, which it never changes (they are
 * callee-saved); another suspends it again and again and reads its context
 * with CONTEXT_FLOATING_POINT. Every context must carry the floating-point
 * state and show those values, and the loop must end with them unchanged.
 * Then the same with a loop that makes no call (guest code: the suspend
 * signal's own context). Each phase ends with a SetThreadContext changing
 * xmm15, which the thread must see. */
#include <winsock2.h>
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;

static void check(int ok, const char *what, double got)
{
    if (ok) printf("ok %s (%.0f)\n", what, got);
    else { printf("FAIL %s (%.0f)\n", what, got); failures++; }
}

/* spin(regs, stop, fn, buf): xmm6-xmm15 = regs[0..9], then fn(buf, 256)
 * (or, with no fn, a count of calls_started) until *stop, then
 * regs[10..19] = xmm6-xmm15. */
void spin(unsigned char *regs, volatile LONG *stop, int (WINAPI *fn)(char *, int), char *buf);
__asm__(
    ".globl spin\n"
    "spin:\n"
    "  push %rbx\n  push %rsi\n  push %rdi\n  push %r12\n"
    "  sub $200, %rsp\n"
    "  movdqu %xmm6, 32(%rsp)\n  movdqu %xmm7, 48(%rsp)\n  movdqu %xmm8, 64(%rsp)\n"
    "  movdqu %xmm9, 80(%rsp)\n  movdqu %xmm10, 96(%rsp)\n  movdqu %xmm11, 112(%rsp)\n"
    "  movdqu %xmm12, 128(%rsp)\n  movdqu %xmm13, 144(%rsp)\n  movdqu %xmm14, 160(%rsp)\n"
    "  movdqu %xmm15, 176(%rsp)\n"
    "  mov %rcx, %rbx\n  mov %rdx, %rsi\n  mov %r8, %rdi\n  mov %r9, %r12\n"
    "  movdqu 0(%rbx), %xmm6\n  movdqu 16(%rbx), %xmm7\n  movdqu 32(%rbx), %xmm8\n"
    "  movdqu 48(%rbx), %xmm9\n  movdqu 64(%rbx), %xmm10\n  movdqu 80(%rbx), %xmm11\n"
    "  movdqu 96(%rbx), %xmm12\n  movdqu 112(%rbx), %xmm13\n  movdqu 128(%rbx), %xmm14\n"
    "  movdqu 144(%rbx), %xmm15\n"
    "1:\n"
    "  test %rdi, %rdi\n  jz 2f\n"
    "  mov %r12, %rcx\n  mov $256, %edx\n  call *%rdi\n"
    "  jmp 3f\n"
    "2:\n  lock incl calls_started(%rip)\n"
    "3:\n"
    "  mov (%rsi), %eax\n  test %eax, %eax\n  jz 1b\n"
    "  movdqu %xmm6, 160(%rbx)\n  movdqu %xmm7, 176(%rbx)\n  movdqu %xmm8, 192(%rbx)\n"
    "  movdqu %xmm9, 208(%rbx)\n  movdqu %xmm10, 224(%rbx)\n  movdqu %xmm11, 240(%rbx)\n"
    "  movdqu %xmm12, 256(%rbx)\n  movdqu %xmm13, 272(%rbx)\n  movdqu %xmm14, 288(%rbx)\n"
    "  movdqu %xmm15, 304(%rbx)\n"
    "  movdqu 32(%rsp), %xmm6\n  movdqu 48(%rsp), %xmm7\n  movdqu 64(%rsp), %xmm8\n"
    "  movdqu 80(%rsp), %xmm9\n  movdqu 96(%rsp), %xmm10\n  movdqu 112(%rsp), %xmm11\n"
    "  movdqu 128(%rsp), %xmm12\n  movdqu 144(%rsp), %xmm13\n  movdqu 160(%rsp), %xmm14\n"
    "  movdqu 176(%rsp), %xmm15\n"
    "  add $200, %rsp\n  pop %r12\n  pop %rdi\n  pop %rsi\n  pop %rbx\n  ret\n");

static unsigned char regs[320];
static volatile LONG stop;
static char host[256];
volatile LONG calls_started;

static int WINAPI counted_gethostname(char *buf, int len)
{
    InterlockedIncrement(&calls_started);
    return gethostname(buf, len);
}

static DWORD WINAPI spinner(void *arg)
{
    spin(regs, &stop, arg, host);
    return 0;
}

/* one phase: the spinner with FN (in the Unix call) or without (guest code),
 * 2000 suspended contexts, then xmm15 changed by SetThreadContext */
static void phase(const char *where, int (WINAPI *fn)(char *, int))
{
    unsigned char want15[16];
    char what[128];
    HANDLE t;
    CONTEXT c;
    int i, fp = 0, bad = 0, taken = 0, set = 0;

    stop = 0; calls_started = 0;
    memset(regs + 160, 0, 160);
    t = CreateThread(NULL, 0, spinner, fn, 0, NULL);
    while (calls_started < 100) Sleep(1);
    for (i = 0; i < 2000; i++)
    {
        memset(&c, 0, sizeof c);
        c.ContextFlags = CONTEXT_FULL | CONTEXT_FLOATING_POINT;
        if (SuspendThread(t) == (DWORD)-1) break;
        if (GetThreadContext(t, &c))
        {
            taken++;
            if ((c.ContextFlags & CONTEXT_FLOATING_POINT & ~CONTEXT_AMD64))
            {
                fp++;
                if (memcmp(&c.Xmm6, regs, 160))
                {
                    HMODULE m = NULL;
                    char name[MAX_PATH] = "?";
                    GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                       (void *)c.Rip, &m);
                    if (m) GetModuleFileNameA(m, name, sizeof name);
                    if (bad < 4) printf("bad %d: at %s+%#llx, mxcsr %#lx\n", i, strrchr(name, '\\') ? strrchr(name, '\\') + 1 : name,
                                        (unsigned long long)(c.Rip - (DWORD64)m), (unsigned long)c.MxCsr);
                    bad++;
                }
            }
        }
        ResumeThread(t);
        if (i % 16 == 0) Sleep(0);
    }
    for (i = 0; i < 16; i++) want15[i] = (unsigned char)(0xa5 ^ i);
    memset(&c, 0, sizeof c);
    c.ContextFlags = CONTEXT_FULL | CONTEXT_FLOATING_POINT;
    if (SuspendThread(t) != (DWORD)-1 && GetThreadContext(t, &c))
    {
        memcpy(&c.Xmm15, want15, 16);
        c.ContextFlags = CONTEXT_FLOATING_POINT;
        set = SetThreadContext(t, &c);
        ResumeThread(t);
    }
    InterlockedExchange(&stop, 1);
    WaitForSingleObject(t, INFINITE);
    CloseHandle(t);
    sprintf(what, "%s: contexts taken", where);
    check(taken == 2000, what, taken);
    sprintf(what, "%s: contexts with the floating-point state", where);
    check(fp == taken, what, fp);
    sprintf(what, "%s: of them showing other xmm6-xmm15 than the thread's", where);
    check(bad == 0, what, bad);
    sprintf(what, "%s: xmm6-xmm14 unchanged across the loop", where);
    check(!memcmp(regs + 160, regs, 144), what, calls_started);
    sprintf(what, "%s: xmm15 as SetThreadContext set it", where);
    check(set && !memcmp(regs + 304, want15, 16), what, set);
}

int main(void)
{
    WSADATA wsa;
    int i;

    WSAStartup(MAKEWORD(2, 2), &wsa);
    for (i = 0; i < 160; i++) regs[i] = (unsigned char)(i * 37 + 11);
    check(gethostname(host, sizeof host) == 0, "gethostname works", 0);
    phase("unix call", counted_gethostname);
    phase("guest code", NULL);
    printf("%d failures\n", failures);
    return failures != 0;
}
