// core/truncate-path: truncate(2) by name on a file a shared mapping holds:
// shorter, then longer. The mapping's bytes past the new end read zero, the
// file reads zero where it grew, and stat reports each size; a missing file
// is ENOENT, a directory EISDIR. Built for each ABI (x86-64, aarch64, and
// wasm32, whose numbers are i386's: truncate64).
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void) {
    const char *p = "/tmp/rt-truncate";
    char buf[4096 * 3];
    memset(buf, 'a', sizeof buf);
    int fd = open(p, O_CREAT | O_RDWR | O_TRUNC, 0644);
    if (fd < 0 || write(fd, buf, sizeof buf) != (ssize_t)sizeof buf) return perror("setup"), 1;
    unsigned char *map = mmap(0, sizeof buf, PROT_READ, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) return perror("mmap"), 1;
    struct stat st;
    int r = truncate(p, 4096 + 100);
    stat(p, &st);
    printf("shorter: %d %lld\n", r, (long long)st.st_size);
    printf("mapped: %c %d\n", map[4096 + 99], map[4096 + 100]);
    r = truncate(p, 4096 * 3);
    stat(p, &st);
    printf("longer: %d %lld\n", r, (long long)st.st_size);
    memset(buf, 1, sizeof buf);
    ssize_t n = pread(fd, buf, sizeof buf, 0);
    int zeros = 1;
    for (int i = 4096 + 100; i < (int)sizeof buf; i++) zeros &= buf[i] == 0;
    printf("read: %zd %c %s\n", n, buf[4096 + 99], zeros ? "zeros" : "bytes");
    printf("mapped: %d\n", map[4096 * 2 + 5]);
    errno = 0;
    printf("missing: %d %s\n", truncate("/tmp/rt-no-such", 0), strerror(errno));
    errno = 0;
    printf("directory: %d %s\n", truncate("/tmp", 0), strerror(errno));
    munmap(map, sizeof buf);
    close(fd);
    unlink(p);
    return 0;
}
