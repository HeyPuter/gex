/* Threads inside a process that was forked and then exec'd.
 *
 * Each of those works on its own and the suite already covers them; the
 * combination is what a package manager does. dpkg unpacks a .deb by forking
 * a decompressor, which execs, which starts worker threads -- and that is
 * where `apt-get install` stopped, with the worker trapping on an empty
 * dispatch table slot.
 *
 * The child is reached by exec rather than by fork alone on purpose: an
 * exec'd process has a fresh translation of everything, so its threads are
 * the case where nothing has been translated for the address they start at.
 */
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define WORKERS 4
#define ROUNDS 20000

static char self[4096];

/* Enough arithmetic that a worker runs for a while and reaches code the main
   thread has not, so a table that is missing entries is missing the ones it
   needs. */
static void *work(void *arg) {
    unsigned long seed = (unsigned long)(size_t)arg;
    unsigned long h = 1469598103934665603UL;
    for (int i = 0; i < ROUNDS; i++) {
        seed = seed * 6364136223846793005UL + 1442695040888963407UL;
        h = (h ^ (seed >> 33)) * 1099511628211UL;
    }
    return (void *)(size_t)(h & 0xffff);
}

static int role_worker(void) {
    pthread_t t[WORKERS];
    for (int i = 0; i < WORKERS; i++) {
        if (pthread_create(&t[i], NULL, work, (void *)(size_t)(i + 1)) != 0) {
            perror("pthread_create");
            return 1;
        }
    }
    unsigned long total = 0;
    for (int i = 0; i < WORKERS; i++) {
        void *r;
        pthread_join(t[i], &r);
        total += (unsigned long)(size_t)r;
    }
    printf("worker: %d threads joined, mix=%lu\n", WORKERS, total % 1000);
    return 0;
}

/* A pipeline: this process writes, a forked-and-exec'd child reads. The
   decompressor case in miniature -- the reader has to still be running when
   the writer's bytes arrive, which is what a lost thread breaks. */
static int role_sink(void) {
    unsigned long sum = 0;
    long n;
    char buf[4096];
    while ((n = read(0, buf, sizeof buf)) > 0) {
        for (long i = 0; i < n; i++) {
            sum += (unsigned char)buf[i];
        }
    }
    printf("sink: %ld bytes summing to %lu\n", (long)(sum ? sum / 7 : 0), sum);
    return 0;
}

/* `keep_out` is the pipe's write end, which the child must close: a reader
   that still holds the write end of the pipe it is reading never sees the
   end of it, however diligently the parent closes its own copy. */
static pid_t spawn(const char *role, int stdin_fd, int close_in_child) {
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        exit(1);
    }
    if (pid == 0) {
        if (stdin_fd >= 0) {
            dup2(stdin_fd, 0);
            close(stdin_fd);
        }
        if (close_in_child >= 0) {
            close(close_in_child);
        }
        execl(self, self, role, (char *)NULL);
        perror("execl");
        _exit(127);
    }
    return pid;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    if (n <= 0) {
        snprintf(self, sizeof self, "%s", argv[0]);
    } else {
        self[n] = 0;
    }
    if (argc > 1 && !strcmp(argv[1], "worker")) {
        return role_worker();
    }
    if (argc > 1 && !strcmp(argv[1], "sink")) {
        return role_sink();
    }

    /* Threads in an exec'd child. */
    int status = 0;
    pid_t w = spawn("worker", -1, -1);
    waitpid(w, &status, 0);
    printf("main: worker exited %d\n", WEXITSTATUS(status));

    /* A pipe into an exec'd child, written until it is more than one
       pipe-full so the writer has to wait for the reader to drain it. */
    int fds[2];
    if (pipe(fds) < 0) {
        perror("pipe");
        return 1;
    }
    pid_t s = spawn("sink", fds[0], fds[1]);
    close(fds[0]);
    static char block[8192];
    memset(block, 3, sizeof block);
    for (int i = 0; i < 40; i++) {
        const char *p = block;
        size_t left = sizeof block;
        while (left) {
            ssize_t got = write(fds[1], p, left);
            if (got < 0) {
                if (errno == EINTR) {
                    continue;
                }
                perror("write");
                return 1;
            }
            p += got;
            left -= (size_t)got;
        }
    }
    close(fds[1]);
    waitpid(s, &status, 0);
    printf("main: sink exited %d\n", WEXITSTATUS(status));
    return 0;
}
