/* A 64-bit guest memory for a Windows program (docs/mem64.md): Unity's
 * pattern, 24 reservations of 512 MiB - 4 KiB with no access (12 GiB), a part
 * of each committed and written, then read back; VirtualQuery above 4 GiB;
 * and an executable allocation, which gex keeps below 4 GiB. Run with
 * `gexc run --mem64` (tests/nt/run.sh). x64 only.
 * gex-nt-cc: clang --target=ARCH-w64-mingw32 -fuse-ld=lld */
#include <windows.h>
#include <stdio.h>
#include <stdint.h>

static int failures;
#define CHECK(name, cond) do { int ok_ = (cond); printf("%s %s\n", ok_ ? "ok" : "FAIL", name); if (!ok_) failures++; } while (0)

#define N 24
#define RESERVE (0x20000000u - 0x1000u)
#define USED (1u << 20)

int main(void) {
    char *r[N];
    int made = 0, above = 0;
    for (int i = 0; i < N; i++) {
        r[i] = VirtualAlloc(NULL, RESERVE, MEM_RESERVE, PAGE_NOACCESS);
        if (!r[i]) break;
        made++;
        above += (uintptr_t)r[i] >= (1ull << 32);
        if (!VirtualAlloc(r[i], USED, MEM_COMMIT, PAGE_READWRITE)) break;
        memset(r[i], 'a' + i, USED);
    }
    CHECK("24 reservations of 512 MiB", made == N);
    CHECK("most of them above 4 GiB", above >= N / 2);
    long sum = 0;
    for (int i = 0; i < made; i++)
        for (unsigned j = 0; j < USED; j += 4096) sum += r[i][j];
    CHECK("written and read back", sum == (long)(USED / 4096) * (N * 'a' + N * (N - 1) / 2));
    MEMORY_BASIC_INFORMATION mbi;
    CHECK("VirtualQuery above 4 GiB", made == N && VirtualQuery(r[N - 1], &mbi, sizeof mbi) == sizeof mbi &&
                                         mbi.State == MEM_COMMIT && mbi.AllocationBase == r[N - 1]);
    void *code = VirtualAlloc(NULL, 4096, MEM_RESERVE | MEM_COMMIT, PAGE_EXECUTE_READWRITE);
    CHECK("executable memory below 4 GiB", code && (uintptr_t)code < (1ull << 32));
    for (int i = 0; i < made; i++) VirtualFree(r[i], 0, MEM_RELEASE);
    printf("%d failures\n", failures);
    return failures != 0;
}
