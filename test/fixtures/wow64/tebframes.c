/* tebframes: the guest of test/wow64/tebframes (64-bit). An exception
 * handler that never returns: it jumps back to main (__builtin_longjmp,
 * no unwind), leaving Wine's dispatcher in the middle of calling it, as a
 * runtime that resumes a catch itself does. The stack the dispatcher ran
 * on is then written over, and a second exception is raised and handled.
 *
 * Then the same second exception from a fiber and from the thread, between
 * switches to and from the fiber (each switch made inside an entry into
 * Wine, which ends on the other stack).
 *
 *   tebframes     says "tebframes jumped", "tebframes continued", then
 *                 "tebframes fibers 3" */
#include <windows.h>
#include <stdio.h>

static void *jb[5];

EXCEPTION_DISPOSITION handler_jump(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *disp) {
    if (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)) return ExceptionContinueSearch;
    __builtin_longjmp(jb, 1);
}

EXCEPTION_DISPOSITION handler_continue(EXCEPTION_RECORD *rec, void *frame, CONTEXT *ctx, void *disp) {
    if (rec->ExceptionFlags & (EXCEPTION_UNWINDING | EXCEPTION_EXIT_UNWIND)) return ExceptionContinueSearch;
    return rec->ExceptionCode == 0xe0000002 ? ExceptionContinueExecution : ExceptionContinueSearch;
}

/* raise_jump, raise_continue: RaiseException(code, 0, 0, NULL) from a
 * frame whose handler is handler_jump or handler_continue */
#define RAISER(name, code, handler) \
    __asm__(".text\n\t.globl " #name "\n\t.def " #name "; .scl 2; .type 32; .endef\n" \
            "\t.seh_proc " #name "\n" #name ":\n" \
            "\tsubq $40, %rsp\n\t.seh_stackalloc 40\n\t.seh_endprologue\n" \
            "\t.seh_handler " #handler ", @except\n" \
            "\tmovl $" #code ", %ecx\n\txorl %edx, %edx\n\txorl %r8d, %r8d\n\txorl %r9d, %r9d\n" \
            "\tcall RaiseException\n\tnop\n\taddq $40, %rsp\n\tret\n\t.seh_endproc\n");
RAISER(raise_jump, 0xe0000001, handler_jump)
RAISER(raise_continue, 0xe0000002, handler_continue)
void raise_jump(void);
void raise_continue(void);

/* far down the stack, so that the second raise's frames are above the
 * first one's */
__attribute__((noinline)) static void deep(int n) {
    volatile char pad[4096];
    pad[0] = (char)n;
    if (n) deep(n - 1);
    else raise_jump();
    pad[1] = 0;
}

/* every byte the first raise's frames were on */
__attribute__((noinline)) static void wipe(void) {
    volatile char z[64 * 1024];
    for (unsigned i = 0; i < sizeof z; i++) z[i] = 0;
}

static void *main_fiber;
static int handled;

static void WINAPI fiber_main(void *arg) {
    for (;;) {
        raise_continue();
        handled++;
        SwitchToFiber(main_fiber);
    }
}

int main(void) {
    if (!__builtin_setjmp(jb)) {
        deep(8);
        printf("tebframes not jumped\n");
        return 1;
    }
    printf("tebframes jumped\n");
    fflush(stdout);
    wipe();
    raise_continue();
    printf("tebframes continued\n");
    fflush(stdout);
    main_fiber = ConvertThreadToFiber(NULL);
    void *f = CreateFiber(0, fiber_main, NULL);
    if (!main_fiber || !f) {
        printf("tebframes no fibers\n");
        return 1;
    }
    SwitchToFiber(f);
    raise_continue();
    handled++;
    SwitchToFiber(f);
    printf("tebframes fibers %d\n", handled);
    fflush(stdout);
    return 0;
}
