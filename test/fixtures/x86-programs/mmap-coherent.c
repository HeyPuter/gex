#define _GNU_SOURCE
/* MAP_SHARED file mappings agree with read and write of the same file
 * within a process: a read returns the
 * mapping's unwritten-back stores, a write (pwrite, write, writev) shows in
 * the mapping, a second open of the file maps the same pages (and its
 * munmap leaves the first mapping), ftruncate drops the bytes past the new
 * end, and munmap writes the mapping back. Run in a scratch directory (the
 * cwd). Prints one line per check, then "mmap-coherent ok". */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/uio.h>
#include <unistd.h>

static int fails;

static void check(const char *what, int ok) {
    printf("%s %s\n", what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}

int main(void) {
    char path[64], buf[64];
    snprintf(path, sizeof path, "mmap-coherent.%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || ftruncate(fd, 8192)) { perror("open"); return 2; }
    char *p = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 2; }

    memcpy(p, "hello", 5);
    memset(buf, 0, sizeof buf);
    check("pread sees store", pread(fd, buf, 5, 0) == 5 && !memcmp(buf, "hello", 5));

    check("pwrite shows", pwrite(fd, "world", 5, 100) == 5 && !memcmp(p + 100, "world", 5));

    lseek(fd, 200, SEEK_SET);
    check("write shows", write(fd, "abc", 3) == 3 && !memcmp(p + 200, "abc", 3));

    memcpy(p + 300, "xyz", 3);
    lseek(fd, 300, SEEK_SET);
    memset(buf, 0, sizeof buf);
    check("read sees store", read(fd, buf, 3) == 3 && !memcmp(buf, "xyz", 3));

    memcpy(p + 400, "12", 2);
    memcpy(p + 402, "34", 2);
    char a[2], b[2];
    struct iovec rv[2] = {{a, 2}, {b, 2}};
    lseek(fd, 400, SEEK_SET);
    check("readv sees store", readv(fd, rv, 2) == 4 && !memcmp(a, "12", 2) && !memcmp(b, "34", 2));
    struct iovec wv[2] = {{"AB", 2}, {"CD", 2}};
    lseek(fd, 500, SEEK_SET);
    check("writev shows", writev(fd, wv, 2) == 4 && !memcmp(p + 500, "ABCD", 4));

    int fd2 = open(path, O_RDWR);
    char *q = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, 0);
    if (q == MAP_FAILED) { perror("mmap 2"); return 2; }
    check("second open sees", !memcmp(q, "hello", 5));
    q[10] = 'Q';
    check("second open stores", p[10] == 'Q');
    munmap(q, 8192);
    close(fd2);
    check("first kept", p[10] == 'Q' && !memcmp(p, "hello", 5));

    p[5000] = 7;
    ftruncate(fd, 4096);
    ftruncate(fd, 8192);
    check("truncate drops", p[5000] == 0);

    munmap(p, 8192);
    memset(buf, 0, sizeof buf);
    check("written back", pread(fd, buf, 11, 0) == 11 && !memcmp(buf, "hello", 5) && buf[10] == 'Q');

    /* A fixed shared mapping where the caller says, though the file is
     * mapped elsewhere (Wine maps each view into its own reserved range):
     * at that address, with the file's bytes. */
    char *e = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    /* (below 4 GiB: a 64-bit memory's fixed mappings above are mem64-wx's) */
    char *spot = mmap(0, 8192, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_32BIT, -1, 0);
    e[20] = 'F';
    char *f = mmap(spot, 8192, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED, fd, 0);
    check("fixed elsewhere", f == spot && !memcmp(f, "hello", 5) && f[20] == 'F');
    munmap(f, 8192);
    munmap(e, 8192);

    /* A shared mapping of a read-only descriptor has nothing to write
     * back: Linux refuses it PROT_WRITE; where a store lands anyway (gex
     * lets mprotect through), the next mapping of the file still works. */
    int ro = open(path, O_RDONLY);
    char *r = mmap(0, 4096, PROT_READ, MAP_SHARED, ro, 0);
    if (r != MAP_FAILED && mprotect(r, 4096, PROT_READ | PROT_WRITE) == 0) r[1] = '!';
    char *r2 = mmap(0, 8192, PROT_READ, MAP_PRIVATE, ro, 0);
    check("read-only descriptor", r != MAP_FAILED && r2 != MAP_FAILED && munmap(r, 4096) == 0);
    if (r2 != MAP_FAILED) munmap(r2, 8192);
    close(ro);

    close(fd);
    unlink(path);
    printf("mmap-coherent %s\n", fails ? "FAIL" : "ok");
    return fails != 0;
}
