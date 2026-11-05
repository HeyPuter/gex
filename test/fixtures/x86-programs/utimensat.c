/* utimensat sets a file's modification time (what `touch -d` does), in the
 * working directory and in /tmp: an explicit time, UTIME_OMIT leaving it,
 * and times through a symlink with and without AT_SYMLINK_NOFOLLOW; a
 * missing file is ENOENT. UTIME_NOW and NULL are checked against time(). */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static long mtime_of(const char *p) {
    struct stat st;
    return stat(p, &st) ? -1 : (long)st.st_mtime;
}

static void run(const char *dir) {
    char f[256], l[256];
    snprintf(f, sizeof f, "%s/gex-utimensat-%d", dir, (int)getpid());
    snprintf(l, sizeof l, "%s.link", f);
    unlink(f);
    unlink(l);
    close(open(f, O_CREAT | O_WRONLY, 0644));
    struct timespec t[2] = {{1577836800, 0}, {1577836800, 0}}; /* 2020-01-01 */
    printf("%s: set %d", dir, utimensat(AT_FDCWD, f, t, 0));
    printf(" mtime %ld", mtime_of(f));
    t[1].tv_nsec = UTIME_OMIT;
    t[0].tv_sec = 1;
    printf(" omit %d mtime %ld", utimensat(AT_FDCWD, f, t, 0), mtime_of(f));
    struct timespec t2[2] = {{0, UTIME_OMIT}, {1234567890, 0}};
    symlink(f, l);
    printf(" via-link %d mtime %ld", utimensat(AT_FDCWD, l, t2, 0), mtime_of(f));
    struct timespec t3[2] = {{0, UTIME_OMIT}, {0, UTIME_NOW}};
    long now = (long)time(0);
    utimensat(AT_FDCWD, f, t3, 0);
    printf(" now-ok %d", labs(mtime_of(f) - now) < 5);
    utimensat(AT_FDCWD, f, t, 0);
    utimensat(AT_FDCWD, f, 0, 0);
    printf(" null-ok %d", labs(mtime_of(f) - now) < 5);
    char missing[300];
    snprintf(missing, sizeof missing, "%s.missing", f);
    int r = utimensat(AT_FDCWD, missing, 0, 0);
    printf(" missing %d %s\n", r, r ? strerror(errno) : "-");
    unlink(l);
    unlink(f);
}

int main(void) {
    run(".");
    run("/tmp");
    return 0;
}
