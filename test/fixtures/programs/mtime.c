/* mtime: the guest of test/page/mtime.test.js. */
#include <fcntl.h>
#include <stdio.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
int main(int argc, char **argv) {
    struct stat a, b, c;
    if (stat(argv[1], &a) != 0) { perror("stat"); return 1; }
    int fd = open(argv[1], O_RDONLY);
    if (fd < 0 || fstat(fd, &b) != 0) { perror("fstat"); return 1; }
    printf("bundled %lld %lld\n", (long long)a.st_mtime, (long long)b.st_mtime);
    time_t before = time(0);
    int w = open("/tmp/new", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (w < 0 || write(w, "x", 1) != 1 || fstat(w, &c) != 0) { perror("write"); return 1; }
    printf("written %s\n", c.st_mtime >= before - 2 && c.st_mtime <= time(0) + 2 ? "now" : "WRONG");
    struct timespec t[2] = {{0, UTIME_OMIT}, {1234567890, 0}};
    if (utimensat(AT_FDCWD, "/tmp/new", t, 0) != 0 || stat("/tmp/new", &c) != 0) { perror("utimensat"); return 1; }
    printf("set %lld\n", (long long)c.st_mtime);
    return 0;
}
