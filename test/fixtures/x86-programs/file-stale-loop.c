/* stale-loop-head's loop in a file of code mapped privately: a loop whose
 * head is on one page and whose body is on the next, the head's page made
 * writable, rewritten and made executable again, then the loop run again.
 * The program then execs itself with the file's path, and the second maps
 * the same file at the same address and does the same.
 *
 * A private mapping keeps the file's name for its code after a write, and
 * the name gave the rewritten page its old number back: the body's block
 * jumped straight to its module's old head, which passed its page check, and
 * the loop ran the old code (600002 for 1000000 in one process; in the
 * second, served the first's translations, the old head's constant). The
 * second must also get the file's own code on its first run, whatever the
 * first did to its copy.
 *
 * The code is position-independent and the same bytes in 32- and 64-bit
 * mode: push edi; xor eax, eax; mov edi, N; jmp head ... head: add eax, K;
 * jmp body ... body: dec edi; jnz head; pop edi; ret. */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define PAGE 4096
#define ENTRY 0x40
#define N 200000
#define BLOB 0x70000000UL

static unsigned char *code = (unsigned char *)BLOB;
typedef int (*fn_t)(void);

static void put32(unsigned char *at, int32_t v) {
    memcpy(at, &v, 4);
}

static void head(unsigned char *h, int32_t k) {
    h[0] = 0x05; /* add eax, imm32 */
    put32(h + 1, k);
    h[5] = 0xe9; /* jmp body */
    put32(h + 6, PAGE - 10);
}

static int run(void) {
    fn_t f = (fn_t)(uintptr_t)(code + ENTRY);
    return f();
}

int main(int argc, char **argv) {
    char path[256];
    if (argc > 1) {
        snprintf(path, sizeof path, "%s", argv[1]);
    } else {
        const char *dir = getenv("TMPDIR");
        snprintf(path, sizeof path, "%s/gex-served-stale-loop-%d", dir && *dir ? dir : "/tmp", (int)getpid());
        static unsigned char img[2 * PAGE];
        memset(img, 0x90, sizeof img);
        head(img, 3);
        unsigned char *e = img + ENTRY;
        e[0] = 0x57;                   /* push edi */
        e[1] = 0x31; e[2] = 0xc0;      /* xor eax, eax */
        e[3] = 0xbf; put32(e + 4, N);  /* mov edi, N */
        e[8] = 0xe9; put32(e + 9, -(ENTRY + 13)); /* jmp head */
        unsigned char *b = img + PAGE;
        b[0] = 0xff; b[1] = 0xcf;      /* dec edi */
        b[2] = 0x0f; b[3] = 0x85; put32(b + 4, -(PAGE + 8)); /* jnz head */
        b[8] = 0x5f;                   /* pop edi */
        b[9] = 0xc3;                   /* ret */
        int w = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (w < 0 || write(w, img, sizeof img) != (ssize_t)sizeof img) {
            perror("code file");
            return 2;
        }
        close(w);
    }
    int fd = open(path, O_RDONLY);
    if (fd < 0 || mmap(code, 2 * PAGE, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_FIXED, fd, 0) != code) {
        perror("map");
        return 2;
    }
    const char *who = argc > 1 ? "second" : "first";
    printf("%s: %d\n", who, run());
    /* The head's page only: writable, a new constant, executable again. */
    for (int k = 5; k <= 7; k += 2) {
        if (mprotect(code, PAGE, PROT_READ | PROT_WRITE)) return 4;
        head(code, k);
        if (mprotect(code, PAGE, PROT_READ | PROT_EXEC)) return 5;
        printf("%s head %d: %d\n", who, k, run());
        printf("%s head %d again: %d\n", who, k, run());
    }
    fflush(stdout);
    if (argc == 1) {
        char *args[] = {argv[0], path, NULL};
        execv(argv[0], args);
        perror("execv");
        return 2;
    }
    unlink(path);
    return 0;
}
