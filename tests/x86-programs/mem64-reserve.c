/* More address space than a wasm32 guest memory has (docs/mem64.md): Unity's
 * pattern, 24 PROT_NONE reservations of 512 MiB - 4 KiB (12 GiB), a part of
 * each made writable and written, then all read back. Run under
 * `gexc run --mem64` (tests/x86-programs/run.sh passes it for mem64-*); its
 * output must match native's. x86-64 only. */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>

#define N 24
#define RESERVE (0x20000000u - 0x1000u)
#define USED (1u << 20)

int main(void) {
    char *r[N];
    int above = 0;
    for (int i = 0; i < N; i++) {
        r[i] = mmap(NULL, RESERVE, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (r[i] == MAP_FAILED) {
            printf("reservation %d failed\n", i);
            return 1;
        }
        above += (uintptr_t)r[i] >= (1ull << 32);
        if (mprotect(r[i], USED, PROT_READ | PROT_WRITE) != 0) {
            printf("mprotect %d failed\n", i);
            return 1;
        }
        memset(r[i], 'a' + i, USED);
        r[i][USED - 1] = (char)i;
    }
    long sum = 0;
    for (int i = 0; i < N; i++) {
        for (unsigned j = 0; j < USED; j += 4096) sum += r[i][j];
        sum += r[i][USED - 1];
    }
    /* An executable mapping still works (gex places it below 4 GiB). */
    unsigned char *code = mmap(NULL, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    printf("%d reservations, sum %ld, most above 4 GiB: %s, code mapping: %s\n", N, sum,
           above >= N / 2 ? "yes" : "no", code != MAP_FAILED ? "ok" : "failed");
    return 0;
}
