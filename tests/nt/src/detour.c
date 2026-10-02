/* A function's prologue patched at run time, as Microsoft Detours does
 * (noita.exe links it): VirtualProtect the page writable, write a `jmp` to a
 * hook over the first five bytes, restore the protection, flush the
 * instruction cache. Calls made afterwards -- direct, from code compiled
 * before the patch, and through a function pointer -- must reach the hook,
 * and the original once the bytes are put back. With the function tier
 * (`gexc run --cgen`) the direct call is a wasm call between two compiled
 * functions, which is what gex_abi::PATCHED_PAGES exists for. Prints one line
 * per check. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

/* Long enough for a 5-byte jmp; noinline so the calls below stay calls. */
__attribute__((noinline)) int target(int x) {
    volatile int y = x;
    y = y * 3;
    return y + 1;
}

__attribute__((noinline)) int hook(int x) {
    volatile int y = x;
    return y * 100;
}

__attribute__((noinline)) int call_direct(int x) {
    return target(x) + 0;
}

int (*volatile call_through)(int) = target;

static void patch(unsigned char *at, const unsigned char *bytes) {
    DWORD old;
    VirtualProtect(at, 5, PAGE_EXECUTE_READWRITE, &old);
    memcpy(at, bytes, 5);
    VirtualProtect(at, 5, old, &old);
    FlushInstructionCache(GetCurrentProcess(), at, 5);
}

int main(void) {
    unsigned char *t = (unsigned char *)target;
    unsigned char saved[5], jmp[5];
    /* Warm both paths up first, so whatever runs them was built before the
     * patch. */
    int before = 0;
    for (int i = 0; i < 1000; i++) before += call_direct(1) + call_through(1);
    CHECK("before the patch", before == 1000 * 8);
    memcpy(saved, t, 5);
    int rel = (int)((unsigned char *)hook - (t + 5));
    jmp[0] = 0xe9;
    memcpy(jmp + 1, &rel, 4);
    patch(t, jmp);
    CHECK("a direct call reaches the hook", call_direct(2) == 200);
    CHECK("an indirect call reaches the hook", call_through(3) == 300);
    patch(t, saved);
    CHECK("restored: a direct call", call_direct(2) == 7);
    CHECK("restored: an indirect call", call_through(3) == 10);
    printf("%d failures\n", failures);
    return failures;
}
