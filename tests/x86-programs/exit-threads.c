/* A process ends once, whichever of its threads ends it and wherever the
 * others are (L24: process end without the kernel lock).
 *
 * Each round forks a child that starts three threads which park: one in a
 * blocking read of a pipe nobody writes, one in a blocking accept on a unix
 * listener, one in a futex wait nobody wakes. Then one thread ends the
 * process with exit_group:
 *
 * - round kind 0: the main thread;
 * - kind 1: a fourth thread, while main waits in pthread_join;
 * - kind 2: two threads at once, with different codes (either may win);
 * - kind 3: the last thread to leave by a raw exit, the others gone first
 *   (the end is the last one out's, with status 0).
 *
 * The parent holds the read end of a pipe whose only writer is the child.
 * After waitpid it reads it: an end of file means the child's descriptors
 * were closed before its status was published. Then it checks the status. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

#define ROUNDS 24

static int idle_pipe[2];
static int listener;
static atomic_int parked;
static atomic_int go;
static int futex_word;

static void *reader(void *arg) {
    (void)arg;
    char c;
    atomic_fetch_add(&parked, 1);
    read(idle_pipe[0], &c, 1);
    return NULL;
}

static void *acceptor(void *arg) {
    (void)arg;
    atomic_fetch_add(&parked, 1);
    accept(listener, NULL, NULL);
    return NULL;
}

static void *futexer(void *arg) {
    (void)arg;
    atomic_fetch_add(&parked, 1);
    for (;;) syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, NULL, NULL, 0);
    return NULL;
}

static void *ender(void *arg) {
    int code = (int)(long)arg;
    while (!atomic_load(&go)) usleep(1000);
    syscall(SYS_exit_group, code);
    return NULL;
}

static void *leaver(void *arg) {
    (void)arg;
    syscall(SYS_exit, 0);
    return NULL;
}

static void child(int kind, int round) {
    pipe(idle_pipe);
    listener = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path, sizeof a.sun_path, "/tmp/exit-threads-%d-%d", (int)getppid(), round);
    unlink(a.sun_path);
    if (bind(listener, (struct sockaddr *)&a, sizeof a) || listen(listener, 1)) _exit(90);
    unlink(a.sun_path);
    pthread_t t[6];
    if (kind == 3) {
        /* Three threads leave by a raw exit; then main does. */
        for (int i = 0; i < 3; i++) pthread_create(&t[i], NULL, leaver, NULL);
        usleep(20000);
        syscall(SYS_exit, 0);
    }
    pthread_create(&t[0], NULL, reader, NULL);
    pthread_create(&t[1], NULL, acceptor, NULL);
    pthread_create(&t[2], NULL, futexer, NULL);
    while (atomic_load(&parked) < 3) usleep(1000);
    usleep(10000); /* into their calls */
    if (kind == 0) syscall(SYS_exit_group, 40 + round % 4);
    pthread_create(&t[3], NULL, ender, (void *)(long)(50 + round % 4));
    if (kind == 2) pthread_create(&t[4], NULL, ender, (void *)(long)(60 + round % 4));
    atomic_store(&go, 1);
    pthread_join(t[3], NULL);
    _exit(99); /* never: exit_group ended the process */
}

int main(void) {
    int bad = 0;
    for (int round = 0; round < ROUNDS; round++) {
        int kind = round % 4;
        int out[2];
        pipe(out);
        fflush(stdout);
        pid_t pid = fork();
        if (pid == 0) {
            close(out[0]);
            child(kind, round);
        }
        close(out[1]);
        int st;
        if (waitpid(pid, &st, 0) != pid) {
            printf("round %d: waitpid failed (errno %d)\n", round, errno);
            bad++;
            continue;
        }
        char c;
        ssize_t n = read(out[0], &c, 1);
        close(out[0]);
        if (n != 0) {
            printf("round %d: the child's pipe end was still open (read %zd)\n", round, n);
            bad++;
        }
        int ok;
        int code = WIFEXITED(st) ? WEXITSTATUS(st) : -1;
        switch (kind) {
        case 0: ok = code == 40 + round % 4; break;
        case 1: ok = code == 50 + round % 4; break;
        case 2: ok = code == 50 + round % 4 || code == 60 + round % 4; break;
        default: ok = code == 0; break;
        }
        if (!ok) {
            printf("round %d (kind %d): status %#x\n", round, kind, st);
            bad++;
        }
    }
    printf("%d rounds, %d bad\n", ROUNDS, bad);
    return bad != 0;
}
