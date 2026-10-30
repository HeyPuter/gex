/* mem64-pick: the guest of test/page/mem64-pick.test.js. */
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>
extern char **environ;
#define ME "/g/pick"

static const char *flavour(void) {
    void *p = mmap((void *)0x140000000ull, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p != MAP_FAILED && (uintptr_t)p >= (1ull << 32) ? "64" : "32";
}

/* Fork and exec `who NAME [kids]`, with `env` (NULL: this one's). */
static void run(const char *name, const char *kids, char **env) {
    pid_t p = fork();
    if (p == 0) {
        char *av[] = { ME, "who", (char *)name, (char *)kids, 0 };
        execve(ME, av, env ? env : environ);
        _exit(127);
    }
    waitpid(p, 0, 0);
}

static void spawn(const char *name, char **env) {
    pid_t p;
    char *av[] = { ME, "who", (char *)name, 0 };
    if (posix_spawn(&p, ME, 0, 0, av, env ? env : environ) == 0) waitpid(p, 0, 0);
    else printf("%s: posix_spawn failed\n", name);
}

int main(int argc, char **argv) {
    static char *ask64[] = { "GEX_MEM64=1", "HOME=/", 0 };
    static char *ask32[] = { "GEX_MEM64=0", "HOME=/", 0 };
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc >= 3 && !strcmp(argv[1], "who")) {
        printf("%s: %s%s\n", argv[2], flavour(), getenv("GEX_MEM64") ? " (GEX_MEM64 leaked)" : "");
        if (argc >= 4 && argv[3] && !strcmp(argv[3], "kids")) {
            run("its child", 0, 0);
            spawn("its spawned child", 0);
        }
        return 0;
    }
    printf("parent: %s\n", flavour());
    run("asked", "kids", ask64);
    run("sibling", 0, 0);
    spawn("spawned, asked", ask64);
    if (setxattr(ME, "user.gex.mem64", "1", 1, 0)) { printf("setxattr failed\n"); return 1; }
    run("recorded", 0, 0);
    spawn("spawned, recorded", 0);
    run("recorded, asked 32", 0, ask32);
    removexattr(ME, "user.gex.mem64");
    run("record gone", 0, 0);
    return 0;
}
