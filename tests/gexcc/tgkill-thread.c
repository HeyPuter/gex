/* tgkill from another process reaches the named thread, not just its process.
 * A Wine server sends SIGUSR1 to the thread that queued an async so that it
 * fetches its system APC; delivered to another thread of the process, the
 * one parked in a wait never wakes. Prints "ok  name" or "FAIL name". */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(name, cond)                                                                \
    do {                                                                                 \
        printf("%s %s\n", (cond) ? "ok  " : "FAIL", name);                               \
        fflush(stdout);                                                                  \
    } while (0)

static volatile pid_t hit_tid;
static void on_usr1(int s) { hit_tid = syscall(SYS_gettid); }

static int report[2];
static int park[2];
static volatile int done;

static void *waiter(void *p) {
    pid_t me = syscall(SYS_gettid);
    write(report[1], &me, sizeof me);
    char c;
    /* Parked in a read until the signal ends it (no SA_RESTART). */
    int r = read(park[0], &c, 1);
    int e = errno;
    pid_t got = hit_tid;
    int out[3] = {me, got, r < 0 ? e : 0};
    write(report[1], out, sizeof out);
    done = 1;
    return p;
}

int main(void) {
    pipe(report);
    pipe(park);
    pid_t pid = fork();
    if (pid == 0) {
        struct sigaction sa = {0};
        sa.sa_handler = on_usr1;
        sigaction(SIGUSR1, &sa, 0);
        pthread_t th;
        pthread_create(&th, NULL, waiter, NULL);
        /* The main thread keeps making syscalls the whole time, so a
         * process-wide signal would land here. */
        while (!done) syscall(SYS_getpid);
        pthread_join(th, NULL);
        _exit(0);
    }
    pid_t tid;
    read(report[0], &tid, sizeof tid);
    usleep(200000);
    CHECK("tgkill to another process's thread succeeds", syscall(SYS_tgkill, pid, tid, SIGUSR1) == 0);
    int out[3];
    read(report[0], out, sizeof out);
    CHECK("the named thread ran the handler", out[1] == tid);
    CHECK("its blocked read ended with EINTR", out[2] == EINTR);
    int st;
    waitpid(pid, &st, 0);
    CHECK("tgkill to a thread of a reaped process is ESRCH", syscall(SYS_tgkill, pid, tid, SIGUSR1) < 0 && errno == ESRCH);
    return 0;
}
