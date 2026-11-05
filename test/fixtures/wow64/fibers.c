/* Fibers on 32-bit Wine (test/wow64/fibers): kernelbase's switch_fiber
 * for the wasm32 build. Two fibers and the thread's own switch to each other
 * many times; each keeps its locals, a buffer on its own stack, its fiber
 * data and its fiber-local storage across every switch, and one switches
 * from deep in a recursion and comes back to it. Prints "fibers ok". */
#include <windows.h>
#include <stdio.h>
#include <string.h>

#define ROUNDS 2000

static LPVOID main_fiber, fa, fb;
static DWORD fls;
static volatile int turns_a, turns_b, bad;

static void check(int ok, const char *what)
{
    if (!ok && !bad) { bad = 1; printf("fibers: %s\n", what); }
}

static int deep(int n, unsigned char *outer)
{
    unsigned char pad[256];
    memset(pad, (unsigned char)n, sizeof(pad));
    if (n == 0)
    {
        /* switch away from the bottom of a recursion and come back */
        SwitchToFiber(main_fiber);
        return outer[0];
    }
    int r = deep(n - 1, pad);
    for (unsigned i = 0; i < sizeof(pad); i++) if (pad[i] != (unsigned char)n) { check(0, "a frame of the recursion changed"); break; }
    return r + 1;
}

static void WINAPI fiber_a(void *data)
{
    unsigned char mine[4096];
    volatile unsigned sum = 0x1234;
    memset(mine, 0xa5, sizeof(mine));
    check(data == (void *)0xa, "fiber a's data");
    FlsSetValue(fls, (void *)0xaa);
    for (int i = 0; i < ROUNDS; i++)
    {
        turns_a++;
        sum += i;
        SwitchToFiber(fb);
        check(GetCurrentFiber() == fa, "fiber a is not current after a switch to it");
        check(GetFiberData() == (void *)0xa, "fiber a's data after a switch");
        check(FlsGetValue(fls) == (void *)0xaa, "fiber a's fiber-local value");
        check(mine[0] == 0xa5 && mine[sizeof(mine) - 1] == 0xa5, "fiber a's stack buffer");
    }
    check(sum == 0x1234u + (unsigned)(ROUNDS * (ROUNDS - 1) / 2), "fiber a's local");
    check(deep(40, mine) == 40 + 1, "the recursion's result"); /* the bottom reads its caller's frame, of n = 1 */
    SwitchToFiber(main_fiber);
}

static void WINAPI fiber_b(void *data)
{
    unsigned char mine[4096];
    volatile unsigned prod = 1;
    memset(mine, 0x5b, sizeof(mine));
    check(data == (void *)0xb, "fiber b's data");
    FlsSetValue(fls, (void *)0xbb);
    for (;;)
    {
        turns_b++;
        prod = prod * 3 + 1;
        /* every tenth turn by way of the thread's own fiber */
        SwitchToFiber(turns_b % 10 ? fa : main_fiber);
        check(GetCurrentFiber() == fb, "fiber b is not current after a switch to it");
        check(FlsGetValue(fls) == (void *)0xbb, "fiber b's fiber-local value");
        check(mine[0] == 0x5b && mine[sizeof(mine) - 1] == 0x5b, "fiber b's stack buffer");
    }
}

int main(void)
{
    main_fiber = ConvertThreadToFiber((void *)0x1);
    if (!main_fiber) { printf("fibers: ConvertThreadToFiber failed %lu\n", GetLastError()); return 1; }
    fls = FlsAlloc(NULL);
    FlsSetValue(fls, (void *)0x11);
    fa = CreateFiber(64 * 1024, fiber_a, (void *)0xa);
    fb = CreateFiber(64 * 1024, fiber_b, (void *)0xb);
    if (!fa || !fb) { printf("fibers: CreateFiber failed %lu\n", GetLastError()); return 1; }
    int visits = 0;
    while (turns_a < ROUNDS && !bad)
    {
        SwitchToFiber(turns_a ? fb : fa);
        visits++;
        check(GetCurrentFiber() == main_fiber, "the thread's fiber is not current after a switch to it");
        check(FlsGetValue(fls) == (void *)0x11, "the thread's fiber-local value");
    }
    /* fiber a is at the bottom of its recursion: back into it, and it
     * comes back here when the recursion is done (it never returns) */
    if (!bad) SwitchToFiber(fa);
    printf("fibers: %d switches to fiber a, %d to fiber b, %d visits back\n", turns_a, turns_b, visits);
    check(turns_a == ROUNDS, "fiber a's turns");
    DeleteFiber(fb);
    DeleteFiber(fa);
    check(ConvertFiberToThread(), "ConvertFiberToThread");
    if (bad) return 1;
    printf("fibers ok\n");
    return 0;
}
