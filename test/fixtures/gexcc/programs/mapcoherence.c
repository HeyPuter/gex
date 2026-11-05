/* Two processes' MAP_SHARED mappings of one file agree where programs
 * synchronise: a store one makes is seen by the other after the writer gives
 * up a lock and the reader takes one (fcntl record locks, flock), or after
 * the writer's msync and the reader's msync(MS_INVALIDATE) -- SQLite's WAL
 * index (-shm) is read this way by every connection. One line per check,
 * compared with native.
 *  1 child stores under F_SETLK, parent reads under F_SETLK
 *  2 parent stores, child reads (the other direction, after 1)
 *  3 msync, then msync(MS_INVALIDATE)
 *  4 flock
 *  5 a page the reader stored to itself and nobody else changed keeps its
 *    store across the lock (only other processes' write-backs come in)
 *  6 a writer that closes the file instead of unlocking (its record locks
 *    go with the close): its store is seen too */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/file.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *F = "mapcoherence.dat";

static void lk(int fd, short type) {
    struct flock l = {0};
    l.l_type = type;
    l.l_start = 0;
    l.l_len = 0;
    if (fcntl(fd, F_SETLKW, &l) != 0) {
        perror("fcntl");
        exit(2);
    }
}

static volatile int *map(int fd) {
    void *p = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) {
        perror("mmap");
        exit(2);
    }
    return p;
}

static void say(int fd) { write(fd, "x", 1); }
static void hear(int fd) {
    char c;
    if (read(fd, &c, 1) != 1)
        exit(3);
}

int main(void) {
    /* Each line as it is printed: the child's comes after the parent's 5. */
    setvbuf(stdout, 0, _IONBF, 0);
    unlink(F);
    int fd = open(F, O_RDWR | O_CREAT, 0644);
    if (fd < 0 || ftruncate(fd, 8192) != 0)
        return 2;
    volatile int *p = map(fd);
    int to_child[2], to_parent[2];
    if (pipe(to_child) || pipe(to_parent))
        return 2;
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        int cfd = open(F, O_RDWR);
        volatile int *q = map(cfd);
        /* 1 */
        lk(cfd, F_WRLCK);
        q[0] = 42;
        lk(cfd, F_UNLCK);
        say(to_parent[1]);
        /* 2 */
        hear(to_child[0]);
        lk(cfd, F_RDLCK);
        int v2 = q[1];
        lk(cfd, F_UNLCK);
        /* 3 */
        q[2] = 9;
        msync((void *)q, 8192, MS_SYNC);
        say(to_parent[1]);
        /* 4 */
        hear(to_child[0]);
        flock(cfd, LOCK_EX);
        q[1024 + 3] = 5;
        flock(cfd, LOCK_UN);
        say(to_parent[1]);
        /* 5 */
        hear(to_child[0]);
        lk(cfd, F_WRLCK);
        q[1024 + 4] = 6;
        lk(cfd, F_UNLCK);
        say(to_parent[1]);
        /* 6: a second descriptor, write-locked and stored under, then
         * closed with the lock held (the mapping stays) */
        hear(to_child[0]);
        int c2 = open(F, O_RDWR);
        lk(c2, F_WRLCK);
        q[1024 + 5] = 8;
        close(c2);
        say(to_parent[1]);
        hear(to_child[0]);
        printf("2 child reads the parent's store: %d\n", v2);
        fflush(stdout);
        _exit(0);
    }
    /* 1 */
    hear(to_parent[0]);
    lk(fd, F_RDLCK);
    printf("1 parent reads the child's store: %d\n", p[0]);
    lk(fd, F_UNLCK);
    /* 2 */
    lk(fd, F_WRLCK);
    p[1] = 7;
    lk(fd, F_UNLCK);
    say(to_child[1]);
    /* 3 */
    hear(to_parent[0]);
    msync((void *)p, 8192, MS_INVALIDATE);
    printf("3 after msync(MS_INVALIDATE): %d\n", p[2]);
    /* 4 */
    say(to_child[1]);
    hear(to_parent[0]);
    flock(fd, LOCK_SH);
    printf("4 under flock: %d\n", p[1024 + 3]);
    flock(fd, LOCK_UN);
    /* 5: the parent's own store on the first page, the child's on the second */
    p[10] = 11;
    say(to_child[1]);
    hear(to_parent[0]);
    lk(fd, F_RDLCK);
    printf("5 own store kept: %d, the child's: %d\n", p[10], p[1024 + 4]);
    lk(fd, F_UNLCK);
    /* 6 */
    say(to_child[1]);
    hear(to_parent[0]);
    lk(fd, F_RDLCK);
    printf("6 after the writer's close: %d\n", p[1024 + 5]);
    lk(fd, F_UNLCK);
    say(to_child[1]);
    waitpid(c, 0, 0);
    unlink(F);
    return 0;
}
