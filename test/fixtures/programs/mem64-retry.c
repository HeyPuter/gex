/* mem64-retry: the guest of test/core/mem64-retry.test.js. A posix_spawn
 * that asks for a 64-bit memory (GEX_MEM64=1) while the page's fetch of the
 * memory64 runtime fails, again at once (inside the page's backoff), then,
 * once the test says on stdin that the runtime can be had, again after the
 * backoff, when it runs. */
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#define ME "/g/retry"

static void spawn(const char *what) {
    static char *av[] = { ME, "child", 0 };
    static char *env[] = { "GEX_MEM64=1", "HOME=/", 0 };
    pid_t p;
    int st = 0, r = posix_spawn(&p, ME, 0, 0, av, env);
    if (r) {
        printf("%s: %s\n", what, strerror(r));
        return;
    }
    waitpid(p, &st, 0);
    printf("%s: exit %d\n", what, WEXITSTATUS(st));
}

int main(int argc, char **argv) {
    char line[64];
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc > 1) {
        printf("ran\n");
        return 0;
    }
    spawn("first");
    spawn("again at once");
    printf("waiting\n");
    if (!fgets(line, sizeof line, stdin)) return 2;
    struct timespec d = { 1, 500 * 1000 * 1000 };
    nanosleep(&d, 0);
    spawn("after the backoff");
    return 0;
}
