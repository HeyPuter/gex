/* mem64-pe: the guest of test/page/mem64-pe.test.js. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
#define ME "/g/pe"

static const char *flavour(void) {
    void *p = mmap((void *)0x140000000ull, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p != MAP_FAILED && (uintptr_t)p >= (1ull << 32) ? "64" : "32";
}

/* A PE's headers: x86-64 (or i386) PE32+, `base`, characteristics `chars`,
 * a relocation directory of `relocs` bytes. */
static void pe(const char *path, uint64_t base, uint16_t chars, uint32_t relocs, uint16_t machine) {
    unsigned char b[512] = { 'M', 'Z' };
    uint32_t nt = 0x80, size = 0x10000, ndirs = 16;
    uint16_t magic = 0x20b;
    size_t opt = nt + 24;
    FILE *f;
    memcpy(b + 0x3c, &nt, 4);
    memcpy(b + nt, "PE\0\0", 4);
    memcpy(b + nt + 4, &machine, 2);
    memcpy(b + nt + 22, &chars, 2);
    memcpy(b + opt, &magic, 2);
    memcpy(b + opt + 24, &base, 8);
    memcpy(b + opt + 56, &size, 4);
    memcpy(b + opt + 108, &ndirs, 4);
    memcpy(b + opt + 112 + 5 * 8 + 4, &relocs, 4);
    if (!(f = fopen(path, "wb")) || fwrite(b, 1, sizeof(b), f) != sizeof(b) || fclose(f)) {
        printf("%s: write failed\n", path);
        exit(1);
    }
}

/* Exec `who NAME` with WINEGEX_IMAGE=image (and `more`), as Wine execs its
 * loader for a Windows program. */
static void run(const char *name, const char *image, const char *more) {
    pid_t p = fork();
    if (p == 0) {
        char img[256];
        char *av[] = { ME, "who", (char *)name, 0 };
        char *env[] = { img, "HOME=/", (char *)more, 0 };
        snprintf(img, sizeof(img), "WINEGEX_IMAGE=%s", image);
        execve(ME, av, env);
        _exit(127);
    }
    waitpid(p, 0, 0);
}

/* Exec this program as Wine's loader, /tmp/wine, on DOS path `dos` in the
 * prefix /tmp/pfx (its c: is /tmp), as `wine C:\\app.exe` by hand. */
static void by_hand(const char *name, const char *dos) {
    pid_t p = fork();
    if (p == 0) {
        char *av[] = { "/tmp/wine", (char *)dos, (char *)name, 0 };
        char *env[] = { "WINEPREFIX=/tmp/pfx", "HOME=/", 0 };
        execve("/tmp/wine", av, env);
        _exit(127);
    }
    waitpid(p, 0, 0);
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc >= 3 && strstr(argv[0], "/wine")) {
        printf("%s: %s\n", argv[2], flavour());
        return 0;
    }
    if (argc >= 3 && !strcmp(argv[1], "who")) {
        printf("%s: %s%s\n", argv[2], flavour(), getenv("WINEGEX_IMAGE") ? " (WINEGEX_IMAGE leaked)" : "");
        return 0;
    }
    pe("/tmp/fixed.exe", 0x140000000ull, 0x23, 0, 0x8664);
    pe("/tmp/norelocdir.exe", 0x140000000ull, 0x22, 0, 0x8664);
    pe("/tmp/movable.exe", 0x140000000ull, 0x22, 0x200, 0x8664);
    pe("/tmp/low.exe", 0x400000, 0x23, 0, 0x8664);
    pe("/tmp/i386.exe", 0x140000000ull, 0x23, 0, 0x14c);
    printf("parent: %s\n", flavour());
    run("fixed high", "/tmp/fixed.exe", 0);
    run("no reloc directory", "/tmp/norelocdir.exe", 0);
    run("relocatable", "/tmp/movable.exe", 0);
    run("fixed low", "/tmp/low.exe", 0);
    run("not x86-64", "/tmp/i386.exe", 0);
    run("fixed high, asked 32", "/tmp/fixed.exe", "GEX_MEM64=0");
    run("missing", "/tmp/none.exe", 0);
    mkdir("/tmp/pfx", 0755);
    mkdir("/tmp/pfx/dosdevices", 0755);
    if (symlink("/tmp", "/tmp/pfx/dosdevices/c:") || symlink(ME, "/tmp/wine")) { printf("symlink failed\n"); return 1; }
    by_hand("by hand, fixed high", "C:\\fixed.exe");
    by_hand("by hand, relocatable", "C:\\movable.exe");
    return 0;
}
