/* The process's ids from several threads at once (L22c: the credential,
 * session and process group calls without the kernel lock, and the ids
 * every other call reads).
 *
 * One thread changes the process's uid and gid (libc's setuid and setgid,
 * which are process-wide). After each change, two other threads check in
 * turn that they see the new ids:
 * - getuid, geteuid, getgid and getresuid;
 * - the owner of a file they create and stat;
 * - the sender's uid on a signal they send their own process with kill
 *   and take with sigtimedwait.
 * Meanwhile another thread stats its own file and asks getpgid and getsid
 * over and over.
 *
 * As an unprivileged process on Linux the changes are refused, and the
 * checks hold for the ids it keeps; under gex they are granted. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 40

static atomic_int bad, stop;
/* The round the setter has finished and the ids it then had, and how many
 * checkers have finished it. */
static atomic_int round_set = -1, checked;
static atomic_uint want_uid, want_gid;

static void fail(const char *what, long n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%ld, errno %d)\n", what, n, errno);
}

static void *setter(void *arg) {
    (void)arg;
    for (int r = 0; r < ROUNDS; r++) {
        /* Refused where unprivileged; either way, what follows is checked
         * against what this thread now has. */
        (void)setgid(2000 + r % 5);
        (void)setuid(1000 + r % 7);
        atomic_store(&want_uid, getuid());
        atomic_store(&want_gid, getgid());
        atomic_store(&checked, 0);
        atomic_store(&round_set, r);
        while (atomic_load(&checked) < 2) sched_yield();
    }
    atomic_store(&stop, 1);
    return 0;
}

static void check(int id, int r) {
    uid_t u = atomic_load(&want_uid);
    gid_t g = atomic_load(&want_gid);
    if (getuid() != u || geteuid() != u) fail("getuid", (long)getuid());
    if (getgid() != g) fail("getgid", (long)getgid());
    uid_t ru, eu, su;
    if (getresuid(&ru, &eu, &su) || eu != u) fail("getresuid", (long)eu);
    char path[64];
    snprintf(path, sizeof path, "/tmp/cred-threads-%d-%d", (int)getpid(), id);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    struct stat st;
    if (fd < 0 || fstat(fd, &st)) fail("open and stat", r);
    else if (st.st_uid != u) fail("file owner", (long)st.st_uid);
    if (fd >= 0) close(fd);
    unlink(path);
    if (kill(getpid(), SIGUSR1)) fail("kill", r);
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    siginfo_t si;
    struct timespec ts = { 5, 0 };
    if (sigtimedwait(&set, &si, &ts) != SIGUSR1) fail("sigtimedwait", r);
    else if (si.si_uid != u || si.si_pid != getpid()) fail("signal's sender uid", (long)si.si_uid);
}

static void *checker(void *arg) {
    int id = (int)(long)arg;
    for (int r = 0; r < ROUNDS; r++) {
        while (atomic_load(&round_set) < r) sched_yield();
        /* One checker at a time: each takes the one signal it sends. */
        while (atomic_load(&checked) != id) sched_yield();
        check(id, r);
        atomic_fetch_add(&checked, 1);
    }
    return 0;
}

static void *asker(void *arg) {
    (void)arg;
    char path[64];
    snprintf(path, sizeof path, "/tmp/cred-threads-%d-asker", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) { fail("asker open", 0); return 0; }
    pid_t pg = getpgid(0), sid = getsid(0);
    while (!atomic_load(&stop)) {
        struct stat st;
        if (fstat(fd, &st)) fail("asker stat", 0);
        if (getpgid(0) != pg || getsid(0) != sid || getpgrp() != pg) fail("group or session", (long)getpgid(0));
    }
    close(fd);
    unlink(path);
    return 0;
}

int main(void) {
    /* Every thread blocks SIGUSR1, so the one a checker sends its process
     * waits for that checker's sigtimedwait. */
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &set, 0);
    pthread_t t[4];
    pthread_create(&t[0], 0, checker, (void *)0L);
    pthread_create(&t[1], 0, checker, (void *)1L);
    pthread_create(&t[2], 0, asker, 0);
    pthread_create(&t[3], 0, setter, 0);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    printf("rounds %d bad %d\n", ROUNDS, atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
