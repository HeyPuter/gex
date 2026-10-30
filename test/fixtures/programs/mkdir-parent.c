/* mkdir-parent: the guest of test/page/mkdir-parent.test.js. */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <dirent.h>
int main(void) {
    int bad = 0;
    int r = mkdir("/t/new/state", 0700);
    printf("mkdir /t/new/state with no /t/new: %s\n", r == 0 ? "made" : strerror(errno));
    bad |= !(r == -1 && errno == ENOENT);
    r = mkdir("/t/f/x", 0700);
    printf("mkdir under a file: %s\n", r == 0 ? "made" : strerror(errno));
    bad |= !(r == -1 && errno == ENOTDIR);
    /* what mkpath does next */
    if (mkdir("/t/new", 0700) || mkdir("/t/new/state", 0700)) { printf("mkpath: %s\n", strerror(errno)); bad = 1; }
    struct stat st;
    bad |= stat("/t/new/state/..", &st) != 0;
    DIR *d = opendir("/t/new");
    int seen = 0; struct dirent *e;
    while (d && (e = readdir(d))) seen |= !strcmp(e->d_name, "state");
    if (d) closedir(d);
    bad |= !seen;
    printf("%s\n", bad ? "FAIL" : "ok");
    return bad;
}
