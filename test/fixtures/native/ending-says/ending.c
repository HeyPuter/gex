/* ending: the guest of test/native/ending-says. Starts N children one
 * after another, child i running /g/c<i> (a copy of this program) as
 * "short": a 3.5 GiB reservation the lowered memory has no room for, then
 * abort(). gex says, at its default level and from the ending process's
 * own Worker, that each needs a 64-bit memory.
 *
 *   ending N      N children, then "ending done"
 *   ending short  the reservation, then abort()
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 0;

    if (argc > 1 && !strcmp(argv[1], "short")) {
        if (mmap(NULL, (size_t)7 << 29, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) != MAP_FAILED) return 0;
        abort();
    }
    for (int i = 0; i < n; i++) {
        char path[32];
        pid_t pid;

        snprintf(path, sizeof path, "/g/c%d", i);
        fflush(stdout);
        pid = fork();
        if (pid == 0) {
            char *args[] = { path, "short", NULL };
            execv(path, args);
            _exit(127);
        }
        waitpid(pid, NULL, 0);
    }
    printf("ending done\n");
    return 0;
}
