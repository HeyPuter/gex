// core/mount-tmpfs: a native (x86-64 or aarch64) program's mount(2) and
// umount2(2), by their raw numbers: a tmpfs mounted over a directory, a file
// written in it and seen there, the mount in /proc/self/mounts, the
// directory empty again once it is unmounted, and a second unmount EINVAL.
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int in_mounts(const char *dir) {
    char line[512];
    FILE *f = fopen("/proc/self/mounts", "r");
    if (!f) return -1;
    int seen = 0;
    while (fgets(line, sizeof line, f)) {
        char src[128], at[256], type[64];
        if (sscanf(line, "%127s %255s %63s", src, at, type) == 3 && !strcmp(at, dir) && !strcmp(type, "tmpfs")) seen = 1;
    }
    fclose(f);
    return seen;
}

int main(void) {
    const char *dir = "/mnt/rt-tmpfs";
    mkdir("/mnt", 0755);
    mkdir(dir, 0755);
    long r = syscall(SYS_mount, "none", dir, "tmpfs", 0L, (void *)0);
    printf("mount: %ld %s\n", r, r ? strerror(errno) : "ok");
    int fd = open("/mnt/rt-tmpfs/f", O_CREAT | O_WRONLY, 0644);
    printf("write: %s\n", fd >= 0 && write(fd, "x", 1) == 1 ? "ok" : strerror(errno));
    if (fd >= 0) close(fd);
    printf("listed: %d\n", in_mounts(dir));
    r = syscall(SYS_umount2, dir, 0L);
    printf("umount: %ld %s\n", r, r ? strerror(errno) : "ok");
    printf("gone: %s\n", access("/mnt/rt-tmpfs/f", F_OK) ? "yes" : "no");
    printf("listed: %d\n", in_mounts(dir));
    r = syscall(SYS_umount2, dir, 0L);
    printf("umount again: %ld %s\n", r, r ? strerror(errno) : "ok");
    return 0;
}
