/* Code mapped where another process had a different library.
 *
 * The parent runs f in /liba.so and starts a fresh process of this program,
 * which holds the range the parent had liba at (so its liba lands
 * elsewhere), runs f there, then maps /libb.so's f over the held range
 * exactly where the parent's liba had its f and calls it. libb's f must run,
 * not liba's: nothing executable was mapped there before.
 *
 * Built twice more with -DLIB=3 -DADD=1 (liba) and -DLIB=5 -DADD=2 (libb). */
#ifdef LIB
int f(int x) { return x * LIB + ADD; }
#else
#define _GNU_SOURCE
#include <dlfcn.h>
#include <elf.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define SPAN 0x40000UL

int main(int argc, char **argv) {
    if (argc == 1) {
        void *h = dlopen("/liba.so", RTLD_NOW);
        int (*f)(int) = h ? (int (*)(int))dlsym(h, "f") : 0;
        if (!f) { printf("dlopen: %s\n", dlerror()); return 1; }
        int s = 0;
        for (int i = 0; i < 1000; i++) s += f(i);
        Dl_info di;
        dladdr((void *)f, &di);
        printf("parent: f at %p, base %p, sum %d\n", (void *)f, di.dli_fbase, s);
        fflush(stdout);
        char a[32], b[32];
        snprintf(a, sizeof a, "%lx", (unsigned long)f);
        snprintf(b, sizeof b, "%lx", (unsigned long)di.dli_fbase);
        pid_t p = fork();
        if (p == 0) {
            execl("/main", argv[0], a, b, (char *)0);
            _exit(127);
        }
        int st;
        waitpid(p, &st, 0);
        return WIFEXITED(st) ? WEXITSTATUS(st) : 99;
    }
    unsigned long pf = strtoul(argv[1], 0, 16), base = strtoul(argv[2], 0, 16);
    /* Where the parent had liba: taken here, so liba lands elsewhere. */
    void *hold = mmap((void *)base, SPAN, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    if (hold != (void *)base) { printf("child: hold at %lx failed\n", base); return 2; }
    void *h = dlopen("/liba.so", RTLD_NOW);
    int (*f)(int) = h ? (int (*)(int))dlsym(h, "f") : 0;
    if (!f || (unsigned long)f == pf) { printf("child: liba at %p\n", (void *)f); return 3; }
    int s = 0;
    for (int i = 0; i < 1000; i++) s += f(i);
    /* libb's f where the parent's liba had its f, over the held range. */
    unsigned long fv = pf - base;
    int fd = open("/libb.so", O_RDONLY);
    Elf64_Ehdr eh;
    pread(fd, &eh, sizeof eh, 0);
    Elf64_Phdr ph[16];
    pread(fd, ph, sizeof(Elf64_Phdr) * eh.e_phnum, eh.e_phoff);
    long off = -1;
    for (int i = 0; i < eh.e_phnum; i++)
        if (ph[i].p_type == PT_LOAD && (ph[i].p_flags & PF_X) && fv >= ph[i].p_vaddr && fv < ph[i].p_vaddr + ph[i].p_filesz)
            off = fv - ph[i].p_vaddr + ph[i].p_offset;
    if (off < 0) { printf("child: no text for %lx in libb\n", fv); return 4; }
    long pg = sysconf(_SC_PAGESIZE);
    void *at = mmap((void *)(pf - off % pg), 2 * pg, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_FIXED, fd, off - off % pg);
    if (at == MAP_FAILED) { printf("child: map libb failed\n"); return 5; }
    int (*g)(int) = (int (*)(int))pf;
    int got = g(7);
    printf("child: liba's f at %p, sum %d; libb's f at %lx: f(7) = %d (want 37)\n", (void *)f, s, pf, got);
    return got == 37 ? 0 : 1;
}
#endif
