/* outgrow: the guest of test/native/outgrow. Reserves 3.5 GiB in one
 * mapping, which a 32-bit memory has no room for and a 64-bit one has.
 *
 *   outgrow crash      no room: says "outgrow short", then aborts
 *   outgrow fallback   no room: maps 64 MiB instead and exits 0
 *   outgrow runtime    writes a million small functions, fills the rest
 *                      of its memory with 1 MiB mappings (each too small
 *                      to count as falling short), says "outgrow runtime
 *                      full", then calls the functions one by one: their
 *                      translations take gex's own memory, which has no
 *                      room left, so it never says "outgrow runtime ran"
 *   outgrow twice M    runs `outgrow M` twice, one after the other (an
 *                      exec each, as a shell would), and says how each
 *                      ended and whether the program is recorded for a
 *                      64-bit memory (its user.gex.mem64 attribute)
 *
 * Each run says "outgrow big high" when it got the 3.5 GiB above 4 GiB. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>

#define BIG (7ull << 29)

static void once(const char *me, const char *mode) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        char *av[] = { (char *)me, (char *)mode, 0 };
        execv(me, av);
        _exit(127);
    }
    int st = 0;
    waitpid(p, &st, 0);
    char rec[8] = "";
    ssize_t n = getxattr(me, "user.gex.mem64", rec, sizeof rec - 1);
    rec[n > 0 ? n : 0] = 0;
    if (WIFSIGNALED(st))
        printf("outgrow child signal %d, record %s\n", WTERMSIG(st), n > 0 ? rec : "none");
    else
        printf("outgrow child exit %d, record %s\n", WEXITSTATUS(st), n > 0 ? rec : "none");
}

int main(int argc, char **argv) {
    if (argc > 2 && !strcmp(argv[1], "twice")) {
        once(argv[0], argv[2]);
        once(argv[0], argv[2]);
        return 0;
    }
    const char *mode = argc > 1 ? argv[1] : "crash";
    if (!strcmp(mode, "runtime")) {
        void *hi = mmap((void *)(1ull << 33), 1 << 20, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if (hi != MAP_FAILED && (uintptr_t)hi >= (1ull << 32)) {
            printf("outgrow big high\n");
            return 0;
        }
        /* mov eax, i; ret -- each at its own 16 bytes */
        enum { FUNCS = 1 << 20 };
        unsigned char *code = mmap(0, FUNCS * 16, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (code == MAP_FAILED) return 2;
        for (unsigned i = 0; i < FUNCS; i++) {
            unsigned char *f = code + i * 16;
            f[0] = 0xb8;
            memcpy(f + 1, &i, 4);
            f[5] = 0xc3;
        }
        mprotect(code, FUNCS * 16, PROT_READ | PROT_EXEC);
        while (mmap(0, 1 << 20, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) != MAP_FAILED)
            ;
        printf("outgrow runtime full\n");
        fflush(stdout);
        unsigned sum = 0;
        for (unsigned i = 0; i < FUNCS; i++)
            sum += ((unsigned (*)(void))(code + i * 16))();
        printf("outgrow runtime ran %u\n", sum);
        return 0;
    }
    void *p = mmap(0, BIG, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (p != MAP_FAILED) {
        printf("outgrow big %s\n", (uintptr_t)p >= (1ull << 32) ? "high" : "low");
        return 0;
    }
    if (!strcmp(mode, "crash")) {
        printf("outgrow short\n");
        fflush(stdout);
        abort();
    }
    p = mmap(0, 64 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    printf("outgrow fallback %s\n", p != MAP_FAILED ? "ok" : "failed");
    return 0;
}
