/* Fibers: ConvertThreadToFiber, CreateFiber, SwitchToFiber back and forth
 * (the fiber's parameter, GetFiberData, GetCurrentFiber), state kept across
 * switches on each fiber's own stack, fiber-local storage with its
 * callback at DeleteFiber, and ConvertFiberToThread. On native Wine
 * (gex W64) a switch is the guest's whole register state, saved and
 * loaded by kernelbase. */
#include <windows.h>
#include <stdio.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

static void *main_fiber, *ping_fiber, *pong_fiber;
static DWORD fls;
static int fls_freed, ping_count, pong_count, param_ok, data_ok;

static void WINAPI fls_callback(void *data)
{
    if (data == (void *)0x1234) fls_freed++;
}

/* A running sum on the fiber's own stack: survives every switch away and
 * back, with the callee-saved registers the compiler keeps it in. */
static void WINAPI ping(void *param)
{
    volatile unsigned long long sum = 0;
    unsigned int i;
    param_ok = param == (void *)0x5150;
    data_ok = GetFiberData() == (void *)0x5150 && GetCurrentFiber() == ping_fiber;
    FlsSetValue(fls, (void *)0x1234);
    for (i = 0; i < 1000; i++) {
        sum += i;
        ping_count++;
        SwitchToFiber(pong_fiber);
        if (FlsGetValue(fls) != (void *)0x1234) failures++;
    }
    CHECK("a fiber's locals survive 1000 switches", sum == 999ull * 1000 / 2);
    SwitchToFiber(main_fiber);
}

static void WINAPI pong(void *param)
{
    double f = 1.0;
    (void)param;
    for (;;) {
        f *= 1.5;
        pong_count++;
        if (pong_count == 1000) CHECK("floating point survives the switches", f > 1e175 && f < 1e177);
        SwitchToFiber(ping_fiber);
    }
}

int main(void)
{
    fls = FlsAlloc(fls_callback);
    CHECK("FlsAlloc", fls != FLS_OUT_OF_INDEXES);
    main_fiber = ConvertThreadToFiber((void *)0x77);
    CHECK("ConvertThreadToFiber", main_fiber != NULL && GetFiberData() == (void *)0x77);
    ping_fiber = CreateFiber(0, ping, (void *)0x5150);
    pong_fiber = CreateFiber(64 * 1024, pong, NULL);
    CHECK("CreateFiber", ping_fiber != NULL && pong_fiber != NULL);
    SwitchToFiber(ping_fiber);
    CHECK("back on the thread's fiber", GetCurrentFiber() == main_fiber && GetFiberData() == (void *)0x77);
    CHECK("the start routine's parameter", param_ok);
    CHECK("GetFiberData and GetCurrentFiber in a fiber", data_ok);
    CHECK("1000 switches each way", ping_count == 1000 && pong_count == 1000);
    DeleteFiber(ping_fiber);
    CHECK("DeleteFiber ran the fiber's FLS callback", fls_freed == 1);
    DeleteFiber(pong_fiber);
    CHECK("ConvertFiberToThread", ConvertFiberToThread());
    printf("%d failures\n", failures);
    return failures;
}
