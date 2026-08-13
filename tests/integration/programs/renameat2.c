/* renameat2: plain rename, plus the RENAME_NOREPLACE atomic create-or-fail
 * idiom that node's fs and careful file writers rely on. Both are deterministic
 * against QEMU. (RENAME_EXCHANGE is intentionally not tested: whether it works
 * depends on the host filesystem under QEMU, so it is not a portable oracle;
 * gex returns EINVAL for it, which Linux also does on filesystems lacking it.) */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef RENAME_NOREPLACE
#define RENAME_NOREPLACE 1
#endif

static void mk(const char *p, const char *c) {
    int f = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    write(f, c, strlen(c));
    close(f);
}
static const char *rd(const char *p) {
    static char b[64];
    int f = open(p, O_RDONLY);
    if (f < 0) return "<none>";
    int n = read(f, b, 63);
    close(f);
    if (n < 0) n = 0;
    b[n] = 0;
    return b;
}

int main(void) {
    chdir("/tmp");
    unlink("a"); unlink("b"); unlink("c"); unlink("d");
    mk("a", "AAA");

    long r = syscall(SYS_renameat2, AT_FDCWD, "a", AT_FDCWD, "b", 0);
    printf("plain ret=%ld b=%s a=%s\n", r, rd("b"), rd("a"));

    mk("c", "CCC");
    errno = 0;
    r = syscall(SYS_renameat2, AT_FDCWD, "b", AT_FDCWD, "c", RENAME_NOREPLACE);
    printf("noreplace-exist ret=%ld errno=%d c=%s\n", r, r < 0 ? errno : 0, rd("c"));

    errno = 0;
    r = syscall(SYS_renameat2, AT_FDCWD, "b", AT_FDCWD, "d", RENAME_NOREPLACE);
    printf("noreplace-new ret=%ld d=%s b=%s\n", r, rd("d"), rd("b"));

    unlink("c"); unlink("d");
    printf("renameat2 ok\n");
    return 0;
}
