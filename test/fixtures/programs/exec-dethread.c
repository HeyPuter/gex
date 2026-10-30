/* An exec from a second thread while the first is busy in each way a
 * thread can be: joining the exec'ing thread, spinning in translated code
 * with no syscall, blocked reading a pipe nobody writes, and in a futex
 * wait nobody wakes. Linux's de_thread stops the first thread before the
 * new image replaces the old one; the page used to place the new image
 * first, and a thread still running the old code ran it over the new one
 * ("nothing to translate", or a corrupted new program).
 *
 * Each case runs as a child, ROUNDS times, the exec at once and after 20 ms
 * by turns; the child's new program finds none of the old program's writes
 * in its memory, and exits 0.
 * Prints "exec-dethread ok ROUNDS". */
#define _GNU_SOURCE
#include <linux/futex.h>
#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;
static char self[4096];
static int word;
/* What the first thread writes while it runs, and the new program checks
 * is still zero: the same executable, so the same address in both images. */
static volatile unsigned char mark[1 << 16];
static int delay_ms;

static void *exec_later(void *arg)
{
    (void)arg;
    struct timespec t = { 0, delay_ms * 1000 * 1000 };
    if (delay_ms) nanosleep(&t, NULL);
    execl(self, self, "done", (char *)NULL);
    _exit(126);
}

/* The new program: nothing of the old one's threads may write into it. */
static int done(void)
{
    struct timespec t = { 0, 30 * 1000 * 1000 };
    nanosleep(&t, NULL);
    for (size_t i = 0; i < sizeof mark; i++)
        if (mark[i]) { printf("done: the old program wrote into the new one at %zu\n", i); return 4; }
    return 0;
}

static int scenario(const char *how, int delay)
{
    delay_ms = delay;
    pthread_t t;
    int p[2];
    char c;
    if (pipe(p)) return 2;
    pthread_create(&t, NULL, exec_later, NULL);
    if (!strcmp(how, "join")) pthread_join(t, NULL);
    else if (!strcmp(how, "spin")) for (unsigned i = 0;; i++) mark[i & (sizeof mark - 1)] = 0xaa;
    else if (!strcmp(how, "read")) (void)!read(p[0], &c, 1);
    else if (!strcmp(how, "futex")) syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    /* Only reached if the exec did not happen. */
    return 3;
}

int main(int argc, char **argv)
{
    if (readlink("/proc/self/exe", self, sizeof self - 1) <= 0) strcpy(self, argv[0]);
    if (argc > 1 && !strcmp(argv[1], "done")) return done();
    if (argc > 3 && !strcmp(argv[1], "case")) return scenario(argv[2], atoi(argv[3]));
    int rounds = argc > 1 ? atoi(argv[1]) : 8;
    const char *cases[] = { "join", "spin", "read", "futex" };
    for (int i = 1; i <= rounds; i++) {
        for (int k = 0; k < 4; k++) {
            /* At once, while the first thread is still on its way into its
             * call, and later, once it is in it. */
            char *av[] = { self, "case", (char *)cases[k], i & 1 ? "0" : "20", NULL };
            pid_t pid;
            int st;
            if (posix_spawn(&pid, self, NULL, NULL, av, environ)) { printf("spawn failed\n"); return 1; }
            if (waitpid(pid, &st, 0) != pid || !WIFEXITED(st) || WEXITSTATUS(st)) {
                printf("%s round %d: status %#x\n", cases[k], i, st);
                return 1;
            }
        }
    }
    printf("exec-dethread ok %d\n", rounds);
    return 0;
}
