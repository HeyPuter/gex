/* mount and umount2 from several threads at once: each change is the whole
 * process's at once, and none is lost to another made beside it.
 *
 * - Four threads each mount a tmpfs on a directory of their own, make a
 *   file in it, see it, unmount it and see the file gone, 25 times over.
 * - Meanwhile one thread mounts a tmpfs that another fills, and the first
 *   sees the file; the second unmounts it, and the first sees it gone.
 * - umount2 of a directory with nothing mounted on it is EINVAL.
 *
 * Natively it needs a mount namespace of its own: an unprivileged user
 * namespace (unshare -rm), made before any thread. gex has none (EINVAL),
 * and its mounts are the process's anyway. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

#define THREADS 4
#define ROUNDS 25

static char base[64];
static atomic_int bad, mounts, seen, gone, step;

static void fail(const char *what, long n) {
    if (atomic_fetch_add(&bad, 1) < 8) printf("bad: %s (%ld, errno %d)\n", what, n, errno);
}

static int write_file(const char *path, const char *s) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return -1;
    int r = write(fd, s, strlen(s)) == (ssize_t)strlen(s) ? 0 : -1;
    close(fd);
    return r;
}

static int exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}

static void *churn(void *p) {
    long i = (long)p;
    char dir[96], file[128];
    snprintf(dir, sizeof dir, "%s/t%ld", base, i);
    for (int r = 0; r < ROUNDS; r++) {
        if (mount("none", dir, "tmpfs", 0, 0)) { fail("mount", i); continue; }
        atomic_fetch_add(&mounts, 1);
        snprintf(file, sizeof file, "%s/f%d", dir, r);
        if (write_file(file, "x")) fail("write in the mount", i);
        if (exists(file)) atomic_fetch_add(&seen, 1);
        else fail("not seen", i);
        if (umount2(dir, 0)) { fail("umount2", i); continue; }
        if (!exists(file)) atomic_fetch_add(&gone, 1);
        else fail("still there", i);
    }
    return 0;
}

/* Wait for `step` to reach `n`, at most about thirty seconds (a loaded
 * host). */
static int wait_step(int n) {
    for (int i = 0; i < 30000 && atomic_load(&step) < n; i++) usleep(1000);
    return atomic_load(&step) >= n;
}

static void *filler(void *p) {
    (void)p;
    char file[128];
    snprintf(file, sizeof file, "%s/shared/from-filler", base);
    if (!wait_step(1)) { fail("no mount to fill", 0); return 0; }
    if (write_file(file, "y")) fail("fill", 0);
    atomic_store(&step, 2);
    if (!wait_step(3)) { fail("not seen by the mounter", 0); return 0; }
    char dir[96];
    snprintf(dir, sizeof dir, "%s/shared", base);
    if (umount2(dir, 0)) fail("umount2 of another thread's mount", 0);
    atomic_store(&step, 4);
    return 0;
}

int main(void) {
    /* The ids before the namespace: unmapped ones read as the overflow id. */
    int uid = (int)getuid(), gid = (int)getgid();
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) == 0) {
        /* Natively: root in a user namespace of its own, mounts private. */
        char map[64];
        int fd;
        snprintf(map, sizeof map, "0 %d 1", uid);
        if ((fd = open("/proc/self/setgroups", O_WRONLY)) >= 0) { (void)!write(fd, "deny", 4); close(fd); }
        if ((fd = open("/proc/self/uid_map", O_WRONLY)) >= 0) { (void)!write(fd, map, strlen(map)); close(fd); }
        snprintf(map, sizeof map, "0 %d 1", gid);
        if ((fd = open("/proc/self/gid_map", O_WRONLY)) >= 0) { (void)!write(fd, map, strlen(map)); close(fd); }
        mount(0, "/", 0, MS_REC | MS_PRIVATE, 0);
    } else if (errno != EINVAL) {
        printf("no mount namespace here (errno %d)\n", errno);
        return 2;
    }
    snprintf(base, sizeof base, "/tmp/mount-threads-%d", (int)getpid());
    char dir[96];
    if (mkdir(base, 0755)) { printf("mkdir %s: errno %d\n", base, errno); return 1; }
    for (int i = 0; i < THREADS; i++) {
        snprintf(dir, sizeof dir, "%s/t%d", base, i);
        if (mkdir(dir, 0755)) fail("mkdir", i);
    }
    snprintf(dir, sizeof dir, "%s/shared", base);
    if (mkdir(dir, 0755)) fail("mkdir shared", 0);
    if (umount2(dir, 0) == 0 || errno != EINVAL) fail("umount2 of nothing", errno);

    pthread_t t[THREADS], f;
    for (long i = 0; i < THREADS; i++)
        if (pthread_create(&t[i], 0, churn, (void *)i)) fail("pthread_create", i);
    if (pthread_create(&f, 0, filler, 0)) fail("pthread_create filler", 0);

    /* The mounter: main. */
    char file[128];
    snprintf(file, sizeof file, "%s/from-filler", dir);
    int shared = 0;
    if (mount("none", dir, "tmpfs", 0, 0)) fail("mount shared", 0);
    atomic_store(&step, 1);
    if (wait_step(2) && exists(file)) shared++;
    else fail("the filler's file not seen", 0);
    atomic_store(&step, 3);
    if (wait_step(4) && !exists(file)) shared++;
    else fail("the filler's unmount not seen", 0);

    for (int i = 0; i < THREADS; i++) pthread_join(t[i], 0);
    pthread_join(f, 0);
    for (int i = 0; i < THREADS; i++) {
        snprintf(dir, sizeof dir, "%s/t%d", base, i);
        if (rmdir(dir)) fail("rmdir", i);
    }
    snprintf(dir, sizeof dir, "%s/shared", base);
    if (rmdir(dir)) fail("rmdir shared", 0);
    if (rmdir(base)) fail("rmdir base", 0);
    printf("mounts %d seen %d gone %d shared %d bad %d\n", atomic_load(&mounts), atomic_load(&seen), atomic_load(&gone), shared, atomic_load(&bad));
    return atomic_load(&bad) != 0;
}
