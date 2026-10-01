/* Code a program writes and rewrites at run time, as a JIT does (LuaJIT,
 * Noita's): written read-write, made executable, called; made writable
 * again, rewritten, made executable, called -- the new code must run, not a
 * translation of the old. And memory released and allocated again at the
 * same address with different code. Prints one line per check. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

typedef int (*fn)(void);

/* mov eax, imm32; ret -- the same bytes in 32- and 64-bit code. */
static void emit(unsigned char *p, int v) {
    p[0] = 0xb8;
    memcpy(p + 1, &v, 4);
    p[5] = 0xc3;
}

int main(void) {
    DWORD old;
    unsigned char *p = VirtualAlloc(0, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    emit(p, 1);
    CHECK("made executable", VirtualProtect(p, 4096, PAGE_EXECUTE_READ, &old));
    CHECK("the code runs", ((fn)p)() == 1);
    VirtualProtect(p, 4096, PAGE_READWRITE, &old);
    emit(p, 2);
    VirtualProtect(p, 4096, PAGE_EXECUTE_READ, &old);
    FlushInstructionCache(GetCurrentProcess(), p, 6);
    CHECK("rewritten code runs", ((fn)p)() == 2);
    /* Twice more, so a translation cached on the second round is caught. */
    for (int i = 3; i <= 4; i++) {
        VirtualProtect(p, 4096, PAGE_READWRITE, &old);
        emit(p, i);
        VirtualProtect(p, 4096, PAGE_EXECUTE_READ, &old);
    }
    CHECK("and again", ((fn)p)() == 4);
    VirtualFree(p, 0, MEM_RELEASE);
    unsigned char *q = VirtualAlloc(p, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    CHECK("the same address again", q == p);
    if (q) {
        emit(q, 5);
        CHECK("new code at a reused address runs", ((fn)q)() == 5);
    }
    printf("%d failures\n", failures);
    return failures;
}
