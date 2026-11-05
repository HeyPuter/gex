/* Advisory file locks between processes, as sqlite, dpkg and apt take them.
 * One line per check, compared with native.
 *  1 F_SETLK: a child is kept out of its parent's write lock (EAGAIN), and
 *    F_GETLK names it (type, start, len, the parent's pid); beside it is
 *    free; the child holds none of its parent's locks (fork)
 *  2 F_SETLKW blocks until the holder unlocks, then takes it
 * Nothing here depends on how fast a process runs: a child says over a pipe
 * when it holds a lock or is about to wait, a holder keeps its lock until
 * told, a waiter checks that the release was announced before its wait
 * ended, a deadlock's two orders print the same, and a signal comes again
 * until it lands in the wait.
 *  3 a partial unlock splits the lock: the hole is free, the rest held
 *  4 closing any descriptor of the file drops the process's record locks
 *  5 the holder's exit drops them
 *  6 F_SETLKW that would close a cycle of two waiters: EDEADLK (for
 *    whichever of the two waits second; the other then gets the lock)
 *  7 F_SETLKW ended by a signal (no SA_RESTART): EINTR
 *  8 OFD locks: the description's; F_OFD_GETLK says pid -1
 *  9 flock: a blocking LOCK_EX waits for the holder's LOCK_UN; LOCK_NB
 *    says EWOULDBLOCK */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *F = "filelocks.dat";
static pid_t parent;

static int lk(int fd, int cmd, short type, off_t start, off_t len) {
    struct flock l = {0};
    l.l_type = type;
    l.l_whence = SEEK_SET;
    l.l_start = start;
    l.l_len = len;
    return fcntl(fd, cmd, &l) == 0 ? 0 : -errno;
}

static void getlk(const char *what, int fd, int cmd, short type, off_t start, off_t len) {
    struct flock l = {0};
    l.l_type = type;
    l.l_start = start;
    l.l_len = len;
    if (fcntl(fd, cmd, &l) != 0) {
        printf("%s: getlk errno %d\n", what, errno);
        return;
    }
    if (l.l_type == F_UNLCK) {
        printf("%s: unlocked\n", what);
        return;
    }
    const char *who = l.l_pid == parent ? "parent" : l.l_pid == -1 ? "-1" : "other";
    printf("%s: %s %lld+%lld pid %s\n", what, l.l_type == F_WRLCK ? "write" : "read", (long long)l.l_start, (long long)l.l_len, who);
}

static void ms(int n) {
    struct timespec t = {n / 1000, (n % 1000) * 1000000L};
    nanosleep(&t, 0);
}

static int open_f(void) {
    int fd = open(F, O_RDWR | O_CREAT, 0644);
    if (fd < 0) {
        perror("open");
        exit(2);
    }
    return fd;
}

/* A byte on `fd` already, without waiting: the announcement of a release. */
static int ready(int fd) {
    int fl = fcntl(fd, F_GETFL);
    fcntl(fd, F_SETFL, fl | O_NONBLOCK);
    char c;
    int n = read(fd, &c, 1);
    fcntl(fd, F_SETFL, fl);
    return n == 1;
}

/* Run `f` in a child; wait for it. */
static void child(void (*f)(void)) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        f();
        fflush(stdout);
        _exit(0);
    }
    int st;
    waitpid(p, &st, 0);
}

static void c1(void) {
    int fd = open_f();
    printf("1 child setlk read 5+1: %d\n", lk(fd, F_SETLK, F_RDLCK, 5, 1));
    getlk("1 child getlk", fd, F_GETLK, F_WRLCK, 0, 0);
    printf("1 child setlk write 10+10: %d\n", lk(fd, F_SETLK, F_WRLCK, 10, 10));
}

static void c3(void) {
    int fd = open_f();
    getlk("3 hole 45", fd, F_GETLK, F_WRLCK, 45, 1);
    getlk("3 held 10", fd, F_GETLK, F_WRLCK, 10, 1);
    getlk("3 held 70", fd, F_GETLK, F_WRLCK, 70, 1);
}

static void c4(void) {
    int fd = open_f();
    printf("4 child setlk after the other close: %d\n", lk(fd, F_SETLK, F_WRLCK, 0, 0));
}

static void c8(void) {
    int fd = open_f();
    printf("8 child ofd setlk: %d\n", lk(fd, F_OFD_SETLK, F_WRLCK, 0, 1));
    getlk("8 child ofd getlk", fd, F_OFD_GETLK, F_WRLCK, 0, 1);
    getlk("8 child getlk", fd, F_GETLK, F_WRLCK, 0, 1);
}

static void on_alarm(int s) { (void)s; }

int main(void) {
    parent = getpid();
    unlink(F);
    int fd = open_f();
    if (ftruncate(fd, 200) != 0)
        return 2;

    printf("1 parent setlk write 0+10: %d\n", lk(fd, F_SETLK, F_WRLCK, 0, 10));
    child(c1);

    /* 2: the child waits; the parent unlocks once it is about to wait, and
       says so on `rel` first: the wait must not end before that. */
    int pp[2], rel[2];
    if (pipe(pp) != 0 || pipe(rel) != 0)
        return 2;
    fflush(stdout);
    pid_t w = fork();
    if (w == 0) {
        int cfd = open_f();
        close(pp[0]);
        close(rel[1]);
        write(pp[1], "w", 1);
        int r = lk(cfd, F_SETLKW, F_WRLCK, 0, 1);
        char b[64];
        int n = snprintf(b, sizeof b, "2 child setlkw: %d, after the unlock: %s\n", r, ready(rel[0]) ? "yes" : "no");
        write(pp[1], b, n);
        _exit(0);
    }
    close(pp[1]);
    close(rel[0]);
    char b[64];
    read(pp[0], b, 1);
    ms(50);
    printf("2 parent unlocks\n");
    fflush(stdout);
    write(rel[1], "u", 1);
    close(rel[1]);
    lk(fd, F_SETLK, F_UNLCK, 0, 0);
    int n = read(pp[0], b, sizeof b);
    if (n > 0)
        fwrite(b, 1, n, stdout);
    close(pp[0]);
    waitpid(w, 0, 0);

    /* 3: split */
    lk(fd, F_SETLK, F_WRLCK, 0, 100);
    lk(fd, F_SETLK, F_UNLCK, 40, 20);
    child(c3);
    lk(fd, F_SETLK, F_UNLCK, 0, 0);

    /* 4: close any descriptor */
    int fd2 = open_f();
    lk(fd, F_SETLK, F_WRLCK, 0, 0);
    close(fd2);
    child(c4);

    /* 5: the holder's exit */
    if (pipe(pp) != 0 || pipe(rel) != 0)
        return 2;
    fflush(stdout);
    pid_t h = fork();
    if (h == 0) {
        int cfd = open_f();
        close(pp[0]);
        close(rel[1]);
        lk(cfd, F_SETLK, F_WRLCK, 0, 0);
        write(pp[1], "h", 1);
        read(rel[0], b, 1);
        _exit(0);
    }
    close(pp[1]);
    close(rel[0]);
    read(pp[0], b, 1);
    printf("5 while the child holds it: %d\n", lk(fd, F_SETLK, F_WRLCK, 0, 1));
    write(rel[1], "x", 1);
    close(rel[1]);
    close(pp[0]);
    waitpid(h, 0, 0);
    printf("5 after it exits: %d\n", lk(fd, F_SETLK, F_WRLCK, 0, 1));
    lk(fd, F_SETLK, F_UNLCK, 0, 0);

    /* 6: deadlock. The parent holds 0, the child 1 and waits for 0. */
    lk(fd, F_SETLK, F_WRLCK, 0, 1);
    if (pipe(pp) != 0)
        return 2;
    fflush(stdout);
    pid_t d = fork();
    if (d == 0) {
        int cfd = open_f();
        close(pp[0]);
        lk(cfd, F_SETLK, F_WRLCK, 1, 1);
        write(pp[1], "x", 1);
        int r = lk(cfd, F_SETLKW, F_WRLCK, 0, 1);
        char c[64];
        int k = snprintf(c, sizeof c, "6 child setlkw 0 once free: %d\n", r);
        write(pp[1], c, k);
        _exit(0);
    }
    close(pp[1]);
    read(pp[0], b, 1);
    /* Most often the child waits first and the parent's wait is the one
       refused; if the child is late, its own wait is: the same pair. */
    ms(50);
    int r = lk(fd, F_SETLKW, F_WRLCK, 1, 1);
    lk(fd, F_SETLK, F_UNLCK, 0, 0);
    n = read(pp[0], b, sizeof b - 1);
    close(pp[0]);
    waitpid(d, 0, 0);
    b[n > 0 ? n : 0] = 0;
    const char *cr = strstr(b, ": ");
    int rc = cr ? atoi(cr + 2) : 1;
    printf("6 setlkw 1 and 0: %s\n", (r == -EDEADLK && rc == 0) || (r == 0 && rc == -EDEADLK) ? "one EDEADLK, the other 0" : "other");

    /* 7: EINTR */
    lk(fd, F_SETLK, F_WRLCK, 0, 1);
    fflush(stdout);
    pid_t e = fork();
    if (e == 0) {
        struct sigaction sa = {0};
        sa.sa_handler = on_alarm;
        sigaction(SIGALRM, &sa, 0);
        int cfd = open_f();
        struct itimerval it = {{0, 100000}, {0, 100000}};
        setitimer(ITIMER_REAL, &it, 0);
        int r7 = lk(cfd, F_SETLKW, F_WRLCK, 0, 1);
        struct itimerval off = {{0, 0}, {0, 0}};
        setitimer(ITIMER_REAL, &off, 0);
        printf("7 child setlkw with an alarm: %s\n", r7 == -EINTR ? "EINTR" : "other");
        fflush(stdout);
        _exit(0);
    }
    waitpid(e, 0, 0);
    lk(fd, F_SETLK, F_UNLCK, 0, 0);

    /* 8: OFD */
    int ofd = open_f();
    printf("8 parent ofd setlk: %d\n", lk(ofd, F_OFD_SETLK, F_WRLCK, 0, 1));
    child(c8);
    close(ofd);
    printf("8 after its close: %d\n", lk(fd, F_SETLK, F_WRLCK, 0, 1));
    lk(fd, F_SETLK, F_UNLCK, 0, 0);

    /* 9: flock */
    int f1 = open_f();
    printf("9 parent flock ex: %d\n", flock(f1, LOCK_EX) == 0 ? 0 : -errno);
    if (pipe(pp) != 0 || pipe(rel) != 0)
        return 2;
    fflush(stdout);
    pid_t g = fork();
    if (g == 0) {
        close(pp[0]);
        close(rel[1]);
        int cfd = open_f();
        int nb = flock(cfd, LOCK_EX | LOCK_NB) == 0 ? 0 : errno;
        write(pp[1], "n", 1);
        int bl = flock(cfd, LOCK_EX) == 0 ? 0 : -errno;
        char c[96];
        int k = snprintf(c, sizeof c, "9 child nb: %s, blocking: %d, after the unlock: %s\n", nb == EWOULDBLOCK ? "EWOULDBLOCK" : "other", bl,
                         ready(rel[0]) ? "yes" : "no");
        write(pp[1], c, k);
        _exit(0);
    }
    close(pp[1]);
    close(rel[0]);
    read(pp[0], b, 1);
    ms(50);
    printf("9 parent unlocks\n");
    fflush(stdout);
    write(rel[1], "u", 1);
    close(rel[1]);
    flock(f1, LOCK_UN);
    n = read(pp[0], b, sizeof b);
    if (n > 0)
        fwrite(b, 1, n, stdout);
    waitpid(g, 0, 0);
    unlink(F);
    return 0;
}
