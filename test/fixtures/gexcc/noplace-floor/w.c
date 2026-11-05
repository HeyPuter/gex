/* test/gexcc/noplace-floor's wasm32 program: its memory at main, a fork's
 * and a posix_spawn child's; x (translated) spawned and exec'd; an 8 MiB
 * image where the placement window was, checked after the arena grows. */
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
static unsigned char image[8 << 20];
static void image_check(void) {
    memset(image, 0x5a, sizeof image);
    for (int i = 0; i < 64; i++) memset(malloc(1 << 20), 0, 1 << 20);
    size_t bad = 0;
    for (size_t i = 0; i < sizeof image; i++) bad += image[i] != 0x5a;
    printf("image %s\n", bad ? "clobbered" : "intact");
}
static void say(const char *who) {
    printf("%s memory %lu\n", who, (unsigned long)(__builtin_wasm_memory_size(0) * 65536ULL >> 20));
    fflush(stdout);
}
static void spawn(char *const *argv) {
    pid_t pid;
    if (posix_spawn(&pid, argv[0], 0, 0, argv, environ) != 0) { printf("spawn %s failed\n", argv[0]); return; }
    waitpid(pid, 0, 0);
}
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "spawned")) { say("spawned"); return 0; }
    if (argc > 1 && !strcmp(argv[1], "exec")) { execv("/t/x", (char *[]){"/t/x", "exec", 0}); printf("execv failed\n"); return 1; }
    say("first");
    pid_t pid = fork();
    if (pid == 0) { say("forked"); _exit(0); }
    waitpid(pid, 0, 0);
    spawn((char *[]){"/t/w", "spawned", 0});
    spawn((char *[]){"/t/x", "spawn", 0});
    spawn((char *[]){"/t/w", "exec", 0});
    image_check();
    return 0;
}
