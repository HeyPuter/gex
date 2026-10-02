/* A block that starts on one page and runs on into the next, with only the
 * second page rewritten afterwards: `mov eax, imm32` starts 3 bytes before a
 * page boundary, so its immediate's last 2 bytes and the `ret` are on the
 * next page. Called, then the next page alone is made writable, the bytes
 * there changed, made executable again and flushed; called again it must
 * return the new value. Twice: with nothing else on the second page (the
 * block must still see that page rewritten), and with a function of its own
 * there called first (the first page's directory entry is unchanged, so
 * dispatch back to the block must not land on the old block again: that was
 * a hang). Prints one line per check. */
#include <windows.h>
#include <stdio.h>
#include <string.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

typedef int (*fn)(void);

static void one(const char *what, int second_fn) {
    DWORD old;
    char name[96];
    unsigned char *p = VirtualAlloc(0, 8192, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    unsigned char *code = p + 4096 - 3;
    int v = 0x11223344, w = 7;
    code[0] = 0xb8; /* mov eax, imm32 */
    memcpy(code + 1, &v, 4);
    code[5] = 0xc3; /* ret */
    unsigned char *g = p + 4096 + 64; /* mov eax, 7; ret */
    g[0] = 0xb8;
    memcpy(g + 1, &w, 4);
    g[5] = 0xc3;
    VirtualProtect(p, 8192, PAGE_EXECUTE_READ, &old);
    FlushInstructionCache(GetCurrentProcess(), p, 8192);
    fn f = (fn)code;
    int a = 0;
    if (second_fn)
        for (int i = 0; i < 100; i++) a += ((fn)g)() == 7;
    for (int i = 0; i < 100; i++) a += f() == 0x11223344;
    snprintf(name, sizeof name, "%s: the code runs", what);
    CHECK(name, a == (second_fn ? 200 : 100));
    /* Only the second page: the immediate's high half. */
    VirtualProtect(p + 4096, 4096, PAGE_READWRITE, &old);
    p[4096] = 0x66;
    p[4097] = 0x55;
    VirtualProtect(p + 4096, 4096, PAGE_EXECUTE_READ, &old);
    FlushInstructionCache(GetCurrentProcess(), p + 4096, 2);
    snprintf(name, sizeof name, "%s: rewritten second page runs", what);
    CHECK(name, f() == 0x55663344);
    snprintf(name, sizeof name, "%s: and again", what);
    CHECK(name, f() == 0x55663344);
}

int main(void) {
    one("alone", 0);
    one("beside a function", 1);
    printf("%d failures\n", failures);
    return failures;
}
