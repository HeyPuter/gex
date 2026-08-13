/* st_nlink: the hard-link count. A file gains a link per name and loses one per
 * unlink; a directory is 2 + its immediate subdirectories. tar, cp -a and rsync
 * read it to detect hard links, and busybox find reads it to skip stat'ing the
 * children of a leaf directory. gex used to report 1 for everything, which made
 * find drop regular files. Cleans its own names first (no reliance on a fresh
 * /tmp) so QEMU is a stable oracle. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>

static void nl(const char *p) {
    struct stat s;
    if (stat(p, &s) == 0)
        printf("%s nlink=%lu\n", p, (unsigned long)s.st_nlink);
    else
        printf("%s stat-fail\n", p);
}

int main(void) {
    chdir("/tmp");
    unlink("nl1"); unlink("nl2"); unlink("nl3");
    int f = open("nl1", O_CREAT | O_WRONLY, 0644);
    write(f, "x", 1);
    close(f);
    nl("nl1");
    printf("link ret=%d\n", link("nl1", "nl2"));
    nl("nl1"); nl("nl2");
    printf("link ret=%d\n", link("nl1", "nl3"));
    nl("nl1");
    printf("unlink ret=%d\n", unlink("nl2"));
    nl("nl1");
    unlink("nl1"); unlink("nl3");

    /* Directory link counts: empty is 2, each subdirectory adds one. */
    rmdir("nld/a"); rmdir("nld/b"); rmdir("nld");
    mkdir("nld", 0755);
    nl("nld");
    mkdir("nld/a", 0755);
    nl("nld");
    mkdir("nld/b", 0755);
    nl("nld");
    rmdir("nld/a"); rmdir("nld/b"); rmdir("nld");
    printf("nlink ok\n");
    return 0;
}
