/* exec-relative: the guest of test/page/exec-relative.test.js. */
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
static void link_of(char *b, size_t cap) {
    ssize_t n = readlink("/proc/self/exe", b, cap - 1);
    b[n > 0 ? n : 0] = 0;
}
/* er DIR NAME: chdir to DIR and run ./NAME both ways; er WHO: say how
 * /proc/self/exe names this program. */
int main(int argc, char **argv) {
    char me[512], rel[600];
    pid_t p;
    setvbuf(stdout, 0, _IONBF, 0);
    link_of(me, sizeof me);
    if (argc == 2) {
        printf("%s: %s\n", argv[1], me[0] == '/' ? "absolute" : me);
        return 0;
    }
    if (argc != 3 || chdir(argv[1])) { printf("usage: er DIR NAME\n"); return 1; }
    snprintf(rel, sizeof rel, "./%s", argv[2]);
    p = fork();
    if (p == 0) { execl(rel, rel, "execv", (char *)0); _exit(127); }
    waitpid(p, 0, 0);
    char *av[] = { rel, "posix_spawn", 0 };
    if (posix_spawn(&p, rel, 0, 0, av, environ) == 0) waitpid(p, 0, 0);
    else printf("posix_spawn failed\n");
    return 0;
}
