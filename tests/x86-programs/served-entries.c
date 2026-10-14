/* Two programs at one base, with different code on one page, sharing a
 * mapped file of code: the second must run its own code there.
 *
 * Built twice (VAL 1 and, as ARGV0-b, VAL 2; tests/x86-programs/run.sh).
 * The first writes a few bytes of code to a file, maps it at BLOB and
 * calls f, whose direct call into the file puts f's block and the file's
 * in one translation; the page stores it for the file's object. It then
 * execs the second, which maps the same file at the same place and calls
 * into it first: that miss is served the first program's module, which
 * names the first program's page of f. Then it calls its own f.
 *
 * In the page (with GEX_TEST_SERVE_UNMERGED, the served modules installed
 * one at a time) the second program's f page took the first's base, and
 * f answered 8 for 9 (W3: winlogon.exe ran the i386 winedevice.exe's code
 * at 0x400000 and faulted).
 */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#ifndef VAL
#define VAL 1
#endif
#define BLOB 0x70000000UL

/* Alone on its page: the second program reaches this page only through
 * its call of f, after the serve. */
__attribute__((noinline, aligned(4096))) int f(void) {
    int r;
    __asm__ volatile("call 0x70000000" : "=a"(r) : : "rcx", "rdx", "rsi", "rdi", "memory"
#ifdef __x86_64__
                     , "r8", "r9", "r10", "r11"
#endif
    );
    return r + VAL;
}
__attribute__((noinline, aligned(4096))) int after_f(int x) { return x * 3; }

int main(int argc, char **argv) {
    const char *dir = getenv("TMPDIR");
    char path[256];
    if (argc > 1) {
        snprintf(path, sizeof path, "%s", argv[1]);
    } else {
        snprintf(path, sizeof path, "%s/gex-served-entries-%d", dir && *dir ? dir : "/tmp", (int)getpid());
        /* mov eax, 7; ret */
        static const unsigned char code[] = {0xb8, 7, 0, 0, 0, 0xc3};
        int w = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (w < 0 || write(w, code, sizeof code) != (ssize_t)sizeof code) {
            perror("blob");
            return 2;
        }
        close(w);
    }
    int fd = open(path, O_RDONLY);
    if (fd < 0 || mmap((void *)BLOB, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_FIXED, fd, 0) != (void *)BLOB) {
        perror("map");
        return 2;
    }
    int (*volatile blob)(void) = (int (*)(void))BLOB;
    int (*volatile fp)(void) = f;
    if (VAL == 2) {
        /* The file's code first: the miss in it is served. */
        printf("blob %d\n", blob());
    }
    printf("program %d: f %d (want %d)\n", VAL, fp(), 7 + VAL);
    fflush(stdout);
    if (VAL == 1) {
        char next[512];
        snprintf(next, sizeof next, "%s-b", argv[0]);
        char *args[] = {next, path, NULL};
        execv(next, args);
        perror("execv");
        return 2;
    }
    unlink(path);
    return fp() == 7 + VAL ? 0 : 1;
}
