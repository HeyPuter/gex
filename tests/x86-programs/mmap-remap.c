/* A file mapped shared, unmapped and mapped again, as Wine maps an index
 * file's view for each lock it takes. Between the mappings another process
 * changes the file: with pwrite, then through a mapping of its own. The next
 * mapping sees those changes, and its unmap writes back only the pages its
 * own stores changed, so the other process's pages stand. Then the case a
 * stale record of the file would get wrong: the other process changes a
 * page, and the next mapping stores back what the page held before; the
 * file must end with that store. Run in a scratch directory (the cwd).
 * Prints one line per check, then "mmap-remap ok". */
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGES 16
#define SIZE (PAGES * 4096)

static int fails;

static void check(const char *what, int ok) {
    printf("%s %s\n", what, ok ? "ok" : "FAIL");
    if (!ok) fails++;
}

static char *map(int fd) {
    char *p = mmap(0, SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) { perror("mmap"); _exit(2); }
    return p;
}

static char at(int fd, int page) {
    char c = 0;
    return pread(fd, &c, 1, (off_t)page * 4096) == 1 ? c : 0;
}

/* Run `f` in a child that has the file open, and wait for it. */
static void in_child(int fd, void (*f)(int)) {
    pid_t child = fork();
    if (child == 0) {
        f(fd);
        _exit(0);
    }
    int st;
    waitpid(child, &st, 0);
}

static void write_3(int fd) { pwrite(fd, "B", 1, 3 * 4096); }
static void map_5(int fd) { char *p = map(fd); p[5 * 4096] = 'M'; munmap(p, SIZE); }
static void write_7(int fd) { pwrite(fd, "B", 1, 7 * 4096); }

int main(void) {
    char path[64], page[4096];
    snprintf(path, sizeof path, "mmap-remap.%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { perror("open"); return 2; }
    memset(page, 'A', sizeof page);
    for (int i = 0; i < PAGES; i++) pwrite(fd, page, sizeof page, (off_t)i * 4096);

    char *p = map(fd);
    p[0] = 'P';
    munmap(p, SIZE);
    check("first unmap written back", at(fd, 0) == 'P');

    in_child(fd, write_3);
    in_child(fd, map_5);
    p = map(fd);
    check("another process's pwrite seen after a remap", p[3 * 4096] == 'B');
    check("another process's mapping seen after a remap", p[5 * 4096] == 'M');
    p[1 * 4096] = 'Q';
    munmap(p, SIZE);
    check("the remap's store written back", at(fd, 1) == 'Q');
    check("the other process's pages stand", at(fd, 3) == 'B' && at(fd, 5) == 'M' && at(fd, 0) == 'P');

    for (int i = 0; i < 4; i++) {
        p = map(fd);
        p[2 * 4096] = (char)('a' + i);
        munmap(p, SIZE);
    }
    check("unchanged file remapped, each store written back", at(fd, 2) == 'd');

    in_child(fd, write_7);
    p = map(fd);
    check("a page another process changed is seen", p[7 * 4096] == 'B');
    p[7 * 4096] = 'A';
    munmap(p, SIZE);
    check("a store of what the page held before written back", at(fd, 7) == 'A');

    close(fd);
    unlink(path);
    if (fails) return 1;
    printf("mmap-remap ok\n");
    return 0;
}
