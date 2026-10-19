/* user.* extended attributes, as Wine keeps a file's DOS attributes
 * (user.DOSATTRIB): set, get (and its size query), list, remove, the
 * f- forms on a descriptor, the l- forms on a symlink, a directory's,
 * and that a rename and a hard link keep them. Errors as Linux gives
 * them. Run in a scratch directory (the cwd): natively, under gexc and in
 * the page alike. Prints one line per check, then "xattr ok". */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/xattr.h>
#include <unistd.h>

static int fails;

static void check(const char *what, long got, long want) {
    printf("%s %ld%s\n", what, got, got == want ? "" : " FAIL");
    if (got != want) fails++;
}

static long err(long r) { return r < 0 ? -errno : r; }

int main(void) {
    char dir[64], a[96], b[96], c[96], l[96], d[96], buf[256];
    snprintf(dir, sizeof dir, "xattr-test.%d", (int)getpid());
    if (mkdir(dir, 0755)) { perror("mkdir"); return 2; }
    snprintf(a, sizeof a, "%s/a", dir);
    snprintf(b, sizeof b, "%s/b", dir);
    snprintf(c, sizeof c, "%s/c", dir);
    snprintf(l, sizeof l, "%s/l", dir);
    snprintf(d, sizeof d, "%s/d", dir);
    int fd = open(a, O_RDWR | O_CREAT, 0644);
    if (fd < 0) { perror("open"); return 2; }
    mkdir(d, 0755);
    if (symlink("a", l)) { perror("symlink"); return 2; }
    const char *n = "user.DOSATTRIB";
    check("set", err(setxattr(a, n, "0x22", 4, 0)), 0);
    check("size", err(getxattr(a, n, NULL, 0)), 4);
    memset(buf, 0, sizeof buf);
    check("get", err(getxattr(a, n, buf, sizeof buf)), 4);
    check("value", strcmp(buf, "0x22"), 0);
    check("short", err(getxattr(a, n, buf, 2)), -ERANGE);
    check("create", err(setxattr(a, n, "x", 1, XATTR_CREATE)), -EEXIST);
    check("replace", err(setxattr(a, "user.none", "x", 1, XATTR_REPLACE)), -ENODATA);
    check("missing", err(getxattr(a, "user.none", buf, sizeof buf)), -ENODATA);
    check("follow", err(getxattr(l, n, buf, sizeof buf)), 4);
    check("lget", err(lgetxattr(l, n, buf, sizeof buf)), -ENODATA);
    check("lset", err(lsetxattr(l, n, "x", 1, 0)), -EPERM);
    check("rename", err(rename(a, b)), 0);
    check("link", err(link(b, c)), 0);
    check("by link", err(getxattr(c, n, buf, sizeof buf)), 4);
    check("fget", err(fgetxattr(fd, n, buf, sizeof buf)), 4);
    check("second", err(fsetxattr(fd, "user.x", "1", 1, 0)), 0);
    long len = err(listxattr(b, buf, sizeof buf));
    /* Two names in either order: the sum of their lengths, each with its NUL. */
    check("list", len, (long)(strlen(n) + 1 + strlen("user.x") + 1));
    int seen = 0;
    for (long i = 0; i < len; i += strlen(buf + i) + 1)
        seen += !strcmp(buf + i, n) + 2 * !strcmp(buf + i, "user.x");
    check("names", seen, 3);
    int dfd = open(d, O_RDONLY | O_DIRECTORY);
    check("dir", err(fsetxattr(dfd, n, "0x10", 4, 0)), 0);
    check("dir get", err(getxattr(d, n, NULL, 0)), 4);
    check("fremove", err(fremovexattr(fd, n)), 0);
    check("gone", err(getxattr(b, n, buf, sizeof buf)), -ENODATA);
    check("remove", err(removexattr(c, "user.x")), 0);
    check("empty", err(listxattr(c, buf, sizeof buf)), 0);
    check("remove again", err(removexattr(c, "user.x")), -ENODATA);
    close(fd);
    close(dfd);
    unlink(b);
    unlink(c);
    unlink(l);
    rmdir(d);
    rmdir(dir);
    printf("xattr %s\n", fails ? "FAIL" : "ok");
    return fails != 0;
}
