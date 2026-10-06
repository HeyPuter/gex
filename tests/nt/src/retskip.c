/* Small call-free leaves that return somewhere other than their caller's
 * continuation: they add to the return address on the stack (`ret` and
 * `ret 4`). The function tier inlines calls to such leaves into the caller
 * (gex-cgen `inline_leaf_calls`); a `ret` that does not go back to the call
 * leaves through the copy's mismatch exit, which must carry on at the
 * address the leaf left. i386 (x64 just prints the same lines).
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

#ifdef __i386__
/* skip5: returns 5 bytes past its call, over a `mov eax, 1`.
 * skip5n: the same through `ret 4`, popping the argument its caller pushed.
 * plain: returns where it should (the inlined path).
 * Each caller: eax = 0, the call, `mov eax, 1` (5 bytes), eax += 2, ret:
 * 2 when the leaf's address was followed, 3 when it was not. */
__asm__(
    ".text\n"
    "_skip5:\n"
    "  addl $5, (%esp)\n"
    "  ret\n"
    "_skip5n:\n"
    "  addl $5, (%esp)\n"
    "  ret $4\n"
    "_plain:\n"
    "  addl $0, (%esp)\n"
    "  ret\n"
    ".globl _call_skip5\n"
    "_call_skip5:\n"
    "  xorl %eax, %eax\n"
    "  call _skip5\n"
    "  movl $1, %eax\n"
    "  addl $2, %eax\n"
    "  ret\n"
    ".globl _call_skip5n\n"
    "_call_skip5n:\n"
    "  xorl %eax, %eax\n"
    "  pushl $7\n"
    "  call _skip5n\n"
    "  movl $1, %eax\n"
    "  addl $2, %eax\n"
    "  ret\n"
    ".globl _call_plain\n"
    "_call_plain:\n"
    "  xorl %eax, %eax\n"
    "  call _plain\n"
    "  movl $1, %eax\n"
    "  addl $2, %eax\n"
    "  ret\n");
int call_skip5(void);
int call_skip5n(void);
int call_plain(void);
#else
static int call_skip5(void) { return 2; }
static int call_skip5n(void) { return 2; }
static int call_plain(void) { return 3; }
#endif

int main(void) {
    int a = 0, b = 0, c = 0;
    /* Enough calls for every tier on the way. */
    for (int i = 0; i < 20000; i++) {
        a += call_skip5() == 2;
        b += call_skip5n() == 2;
        c += call_plain() == 3;
    }
    CHECK("ret to a rewritten address", a == 20000);
    CHECK("ret 4 to a rewritten address", b == 20000);
    CHECK("ret to the continuation", c == 20000);
    printf("%d failures\n", failures);
    return failures ? 1 : 0;
}
