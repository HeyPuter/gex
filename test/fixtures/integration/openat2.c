/* openat2: the modern openat with an open_how struct. This QEMU build has no
 * openat2 (it answers ENOSYS), so the program falls back to plain openat on
 * ENOSYS -- QEMU then runs the reference openat path while gex runs its real
 * openat2. Identical observable output proves gex's openat2 is equivalent to
 * openat for flags/mode/errno. (resolve is not exercised: gex rejects a
 * non-zero resolve with EINVAL since it cannot enforce constrained lookup, and
 * QEMU cannot run the syscall at all, so it is not a portable oracle.) */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

struct open_how {
    unsigned long long flags;
    unsigned long long mode;
    unsigned long long resolve;
};

static long xopen(const char *p, unsigned long long flags, unsigned long long mode) {
    struct open_how h = {.flags = flags, .mode = mode, .resolve = 0};
    long r = syscall(SYS_openat2, AT_FDCWD, p, &h, sizeof h);
    if (r < 0 && errno == ENOSYS) r = openat(AT_FDCWD, p, (int)flags, (mode_t)mode);
    return r;
}

int main(void) {
    chdir("/tmp");
    unlink("oa2f"); unlink("oa2g");
    int w = open("oa2f", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    write(w, "data42", 6);
    close(w);

    long fd = xopen("oa2f", O_RDONLY, 0);
    char b[16];
    int n = fd >= 0 ? read(fd, b, 15) : -1;
    if (n < 0) n = 0;
    b[n] = 0;
    printf("read ok=%d data=%s\n", fd >= 0, b);
    if (fd >= 0) close(fd);

    long fc = xopen("oa2g", O_CREAT | O_WRONLY | O_TRUNC, 0644);
    printf("create ok=%d\n", fc >= 0);
    if (fc >= 0) {
        write(fc, "x", 1);
        close(fc);
    }

    errno = 0;
    long fm = xopen("nope-xyz", O_RDONLY, 0);
    printf("missing ret=%ld errno=%d\n", fm, fm < 0 ? errno : 0);

    unlink("oa2f"); unlink("oa2g");
    printf("openat2 ok\n");
    return 0;
}
