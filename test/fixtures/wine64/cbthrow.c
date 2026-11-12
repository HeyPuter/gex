/* Guest code leaving a native function's frames by a jump: guest code calls
 * native c_call (through its table index: the entry path), which calls a
 * guest callback back, and the callback jumps (__builtin_longjmp: no unwind)
 * to the guest frame below the native call. Many times over: each must
 * leave the native frames and call-outs behind (no nesting that grows),
 * and a plain call afterwards must still come back normally, also from a
 * jump that leaves two native calls at once. */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include "conv.h"

static int failures;
static void check(int ok, const char *what, long long got)
{
    printf("%s %s (%lld)\n", ok ? "ok" : "FAIL", what, got);
    fflush(stdout);
    if (!ok) failures++;
}

static void *jb[5];
static const struct conv_table *t;

/* the callback's frame on the first and the latest call: the same if
 * nothing nests */
static char *cb_first, *cb_last;

static int jump_out(int x)
{
    cb_last = __builtin_frame_address(0);
    if (!cb_first) cb_first = cb_last;
    __builtin_longjmp(jb, 1);
    return x;
}
static int triple(int x) { return x * 3; }
/* a callback that calls native code again, whose callback jumps out of both */
static int nest(int x) { return t->c_call(jump_out, x); }

static __attribute__((noinline)) int once(int (*cb)(int), int x)
{
    if (__builtin_setjmp(jb)) return -1;
    return t->c_call(cb, x);
}

int main(int argc, char **argv)
{
    int n1 = argc > 1 ? atoi(argv[1]) : 3000, n2 = argc > 2 ? atoi(argv[2]) : 1000;
    HMODULE m = LoadLibraryA("convdll.dll");
    const struct conv_table *(*get)(void);
    int jumped = 0;

    if (!m || !(get = (void *)GetProcAddress(m, "conv_table")))
    {
        printf("FAIL convdll.dll: %lu\n1 failures\n", GetLastError());
        return 1;
    }
    t = get();
    check(t->c_call(triple, 5) == 16, "a plain callback comes back", t->c_call(triple, 5));
    check(once(jump_out, 1) == -1, "one jump out of a callback over a native frame", 0);
    for (int i = 0; i < n1; i++) jumped += once(jump_out, i) == -1;
    check(jumped == n1, "jumps out of a callback over a native frame, many times", jumped);
    check(cb_first && cb_last == cb_first, "the callback's frame on the last jump is the first's (nothing nests)", cb_first - cb_last);
    jumped = 0;
    for (int i = 0; i < n2; i++) jumped += once(nest, i) == -1;
    check(jumped == n2, "jumps out over two native calls, many times", jumped);
    check(t->c_call(triple, 5) == 16, "a plain callback afterwards comes back", t->c_call(triple, 5));
    printf("%d failures\n", failures);
    fflush(stdout);
    return failures != 0;
}
