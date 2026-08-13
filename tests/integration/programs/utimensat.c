/* utimensat existence semantics. gex does not persist file times, but it must
 * report existence honestly: an existing path succeeds, a missing one is
 * ENOENT, the NULL-path (futimens) form checks the descriptor. This was a
 * blanket success, which silently broke busybox `touch` -- touch only creates
 * a new file when utimensat *fails*, so a fake success made it a no-op.
 * Deterministic -> QEMU is the oracle. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void) {
    chdir("/tmp");
    unlink("ue");
    int f = open("ue", O_CREAT | O_WRONLY, 0644);
    close(f);

    errno = 0;
    int a = utimensat(AT_FDCWD, "ue", NULL, 0);
    printf("existing ret=%d errno=%d\n", a, a < 0 ? errno : 0);

    errno = 0;
    int b = utimensat(AT_FDCWD, "no-such-file", NULL, 0);
    printf("missing ret=%d errno=%d\n", b, b < 0 ? errno : 0);

    errno = 0;
    int b2 = utimensat(AT_FDCWD, "no-such-file", NULL, AT_SYMLINK_NOFOLLOW);
    printf("missing-nofollow ret=%d errno=%d\n", b2, b2 < 0 ? errno : 0);

    int fd = open("ue", O_RDONLY);
    errno = 0;
    int c = utimensat(fd, NULL, NULL, 0);
    printf("futimens ret=%d errno=%d\n", c, c < 0 ? errno : 0);
    close(fd);

    errno = 0;
    int d = utimensat(-1, NULL, NULL, 0);
    printf("futimens-badfd ret=%d errno=%d\n", d, d < 0 ? errno : 0);

    unlink("ue");
    printf("utimensat ok\n");
    return 0;
}
