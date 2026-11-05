/* Thread ids and process ids are one numbering, as on Linux: every id names
 * exactly one task, and kill, tgkill and /proc agree on which. gex numbered
 * each process's threads from pid + 1 while pids were global, so a forked
 * child's second thread could have the id of the next process forked
 * (tkill(3) meant pid 2's thread or pid 3).
 *
 * The parent forks a child that starts three threads, each reporting its
 * tid, then forks a second child. Then:
 * - no id is used twice among the parent, its thread, both children and the
 *   first child's threads;
 * - /proc/<tid>/status of each child thread says Tgid is the child;
 * - tgkill(child, tid, 0) finds it, tgkill with the parent or the second
 *   child as tgid is ESRCH;
 * - kill(tid, SIGUSR1) is the child's signal: its handler runs and it exits
 *   0, and the second child is left alone. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define THREADS 3

static int report[2], release_pipe[2], other_pipe[2];
static volatile sig_atomic_t got;
static int bad;

static void fail(const char *what, long v) {
    if (bad++ < 8) printf("bad: %s (%ld)\n", what, v);
}

static pid_t tid(void) { return (pid_t)syscall(SYS_gettid); }

static int tgkill(pid_t tgid, pid_t t, int sig) { return (int)syscall(SYS_tgkill, tgid, t, sig); }

static void on_usr1(int s) { (void)s; got = 1; }

static void *child_thread(void *arg) {
    (void)arg;
    pid_t me = tid();
    if (write(report[1], &me, sizeof me) != sizeof me) _exit(3);
    char c;
    /* Held until the parent is done with this tid. */
    if (read(release_pipe[0], &c, 1) != 1) _exit(4);
    return 0;
}

static void *parent_thread(void *arg) {
    *(pid_t *)arg = tid();
    return 0;
}

static int status_tgid(pid_t t) {
    char path[64], buf[2048];
    snprintf(path, sizeof path, "/proc/%d/status", (int)t);
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    int n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) return -1;
    buf[n] = 0;
    char *p = strstr(buf, "\nTgid:");
    return p ? atoi(p + 6) : -1;
}

int main(void) {
    if (pipe(report) || pipe(release_pipe) || pipe(other_pipe)) return 2;
    pid_t child = fork();
    if (child == 0) {
        struct sigaction sa = { .sa_handler = on_usr1 };
        sigemptyset(&sa.sa_mask);
        sigaction(SIGUSR1, &sa, 0);
        /* Only the main thread takes the signal: the others block it. */
        sigset_t usr1, old;
        sigemptyset(&usr1);
        sigaddset(&usr1, SIGUSR1);
        pthread_sigmask(SIG_BLOCK, &usr1, &old);
        pthread_t t[THREADS];
        for (int i = 0; i < THREADS; i++) pthread_create(&t[i], 0, child_thread, 0);
        pthread_sigmask(SIG_SETMASK, &old, 0);
        while (!got) usleep(1000);
        for (int i = 0; i < THREADS; i++) pthread_join(t[i], 0);
        _exit(0);
    }
    pid_t tids[THREADS];
    for (int i = 0; i < THREADS; i++)
        if (read(report[0], &tids[i], sizeof tids[i]) != sizeof tids[i]) return 3;
    pid_t other = fork();
    if (other == 0) {
        /* Ends when the parent says. */
        char c;
        _exit(read(other_pipe[0], &c, 1) == 1 ? 7 : 6);
    }
    pid_t mine = 0;
    pthread_t pt;
    pthread_create(&pt, 0, parent_thread, &mine);
    pthread_join(pt, 0);

    pid_t ids[4 + THREADS] = { getpid(), mine, child, other };
    for (int i = 0; i < THREADS; i++) ids[4 + i] = tids[i];
    for (int i = 0; i < 4 + THREADS; i++)
        for (int j = i + 1; j < 4 + THREADS; j++)
            if (ids[i] == ids[j]) fail("one id for two tasks", ids[i]);

    for (int i = 0; i < THREADS; i++) {
        if (status_tgid(tids[i]) != child) fail("/proc/<tid>/status Tgid", status_tgid(tids[i]));
        if (tgkill(child, tids[i], 0) != 0) fail("tgkill(child, tid, 0)", errno);
        errno = 0;
        if (tgkill(getpid(), tids[i], 0) == 0 || errno != ESRCH) fail("tgkill(self, child's tid) not ESRCH", errno);
        errno = 0;
        if (tgkill(other, tids[i], 0) == 0 || errno != ESRCH) fail("tgkill(other, child's tid) not ESRCH", errno);
    }

    /* A thread's id with kill is its process's signal. */
    if (kill(tids[1], SIGUSR1) != 0) fail("kill(tid)", errno);
    /* Let the child's threads go. */
    for (int i = 0; i < THREADS; i++) if (write(release_pipe[1], "r", 1) != 1) return 4;
    int st = 0;
    if (waitpid(child, &st, 0) != child || !WIFEXITED(st) || WEXITSTATUS(st) != 0) fail("child's exit", st);
    if (write(other_pipe[1], "x", 1) != 1) return 5;
    if (waitpid(other, &st, 0) != other || !WIFEXITED(st) || WEXITSTATUS(st) != 7) fail("second child's exit", st);
    printf("tasks %d bad %d\n", 4 + THREADS, bad);
    return bad != 0;
}
