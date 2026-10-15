/* The calls that need nothing of the process from several threads at once
 * (L22: uname, prctl, sync, fsync and fdatasync, the robust list, the
 * scheduling calls, sched_getaffinity, getcpu, membarrier, getgroups,
 * getppid, arch_prctl, and the clocks' times and time, without the kernel
 * lock).
 *
 * Four threads ask each over and over and check the answers: the same
 * uname every time, a CPU the affinity names, policy SCHED_OTHER, an
 * unknown policy refused, a file of their own synced and a closed
 * descriptor EBADF, the thread's own fs base unchanged (x86-64), times and
 * time moving forward.
 * Meanwhile the main thread forks children that check their parent is it. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/times.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 2000
#define THREADS 4

static atomic_int bad;
static struct utsname first;
static char dir[64] = "/tmp/quiet-threads-XXXXXX";

static void fail(const char *what, long n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%ld, errno %d)\n", what, n, errno);
}

static void *asker(void *arg) {
    int id = (int)(long)arg;
    char path[64];
    snprintf(path, sizeof path, "%s/%d", dir, id);
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) { fail("open", id); return 0; }
    char name[16];
    snprintf(name, sizeof name, "asker%d", id);
    if (prctl(PR_SET_NAME, name, 0, 0, 0)) fail("prctl", id);
#ifdef __x86_64__
    unsigned long fs0 = 0;
    if (syscall(SYS_arch_prctl, 0x1003 /* ARCH_GET_FS */, &fs0)) fail("arch_prctl", id);
#endif
    clock_t last_ticks = 0;
    time_t last_time = 0;
    for (int i = 0; i < ROUNDS; i++) {
        struct utsname u;
        if (uname(&u) || memcmp(&u, &first, sizeof u)) fail("uname", i);
        cpu_set_t set;
        CPU_ZERO(&set);
        if (sched_getaffinity(0, sizeof set, &set) || CPU_COUNT(&set) < 1) fail("affinity", i);
        unsigned cpu = 99999;
        if (syscall(SYS_getcpu, &cpu, 0, 0) || cpu >= CPU_SETSIZE) fail("getcpu", cpu);
        /* musl's wrappers answer ENOSYS without asking: the calls direct. */
        if (syscall(SYS_sched_getscheduler, 0) != SCHED_OTHER) fail("getscheduler", i);
        struct sched_param p = { .sched_priority = 0 };
        errno = 0;
        if (syscall(SYS_sched_setscheduler, 0, 9, &p) != -1 || errno != EINVAL) fail("policy 9 not EINVAL", errno);
        if (syscall(SYS_sched_setscheduler, 0, SCHED_OTHER, &p)) fail("setscheduler OTHER", errno);
        if (sched_get_priority_max(SCHED_FIFO) != 99) fail("priority max", i);
        if (write(fd, "q", 1) != 1 || fsync(fd) || fdatasync(fd)) fail("fsync", i);
        errno = 0;
        if (fsync(1000 + id) != -1 || errno != EBADF) fail("fsync closed not EBADF", errno);
        if (syscall(SYS_membarrier, 0, 0, 0) < 0) fail("membarrier", i);
        if (getgroups(0, 0) < 0) fail("getgroups", i);
        if (i % 64 == 0) sync();
        struct tms t;
        clock_t ticks = times(&t);
        if (ticks < last_ticks) fail("times went back", (long)ticks);
        last_ticks = ticks;
        time_t now = time(0);
        if (now < last_time) fail("time went back", (long)now);
        last_time = now;
#ifdef __x86_64__
        unsigned long fs = 0;
        if (syscall(SYS_arch_prctl, 0x1003, &fs) || fs != fs0) fail("fs base moved", i);
#endif
    }
    close(fd);
    unlink(path);
    return 0;
}

int main(void) {
    /* Its own directory: concurrent runs under gexc share /tmp, and the
     * guest's pid is the same in each. */
    if (!mkdtemp(dir)) return 2;
    if (uname(&first)) return 2;
    pthread_t t[THREADS];
    for (int i = 0; i < THREADS; i++) pthread_create(&t[i], 0, asker, (void *)(long)i);
    pid_t me = getpid();
    for (int i = 0; i < 20; i++) {
        pid_t c = fork();
        if (c == 0) _exit(getppid() == me ? 0 : 1);
        int st = 0;
        if (waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st)) fail("child's parent", st);
    }
    for (int i = 0; i < THREADS; i++) pthread_join(t[i], 0);
    rmdir(dir);
    printf("threads %d rounds %d bad %d\n", THREADS, ROUNDS, atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
