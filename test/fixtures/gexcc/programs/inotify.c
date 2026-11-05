/* inotify on changes this process makes: one line per check, compared with
 * native. Each watch has its own instance, so no line depends on the order
 * of events between two watches.
 *  1 inotify_init1(IN_NONBLOCK | IN_CLOEXEC): both flags; read of an empty
 *    queue is EAGAIN; not readable to poll; FIONREAD 0
 *  2 a directory watch: create, write (twice: one event), close after
 *    write, chmod, utimes, rename (one cookie, both halves), O_TRUNC open,
 *    ftruncate, mkdir and rmdir (ISDIR), link, symlink, unlink
 *  3 a file watch: attrib, modify, rename (MOVE_SELF), unlink (ATTRIB,
 *    DELETE_SELF, IGNORED)
 *  4 a watched directory removed: DELETE_SELF, IGNORED
 *  5 add_watch: the same inode twice is the same wd; IN_MASK_CREATE on it
 *    is EEXIST; IN_ONLYDIR on a file is ENOTDIR; no such path is ENOENT;
 *    no events is EINVAL; IN_MASK_ADD widens
 *  6 rm_watch: IGNORED queued; again is EINVAL
 *  7 IN_ONESHOT: one event, then IGNORED
 *  8 read: a buffer shorter than the next event is EINVAL; FIONREAD and
 *    poll and epoll see a queued event; names padded
 *  9 a blocking instance's read waits for a change another thread makes
 * 10 a fork's child shares the instance: what it reads, its parent does not
 *    see again */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *D = "inotify.d";

static void path(char *out, const char *name) { snprintf(out, 256, "%s/%s", D, name); }

static void bits(unsigned m) {
    static const struct { unsigned b; const char *n; } t[] = {
        {IN_ACCESS, "ACCESS"}, {IN_MODIFY, "MODIFY"}, {IN_ATTRIB, "ATTRIB"},
        {IN_CLOSE_WRITE, "CLOSE_WRITE"}, {IN_CLOSE_NOWRITE, "CLOSE_NOWRITE"},
        {IN_OPEN, "OPEN"}, {IN_MOVED_FROM, "MOVED_FROM"}, {IN_MOVED_TO, "MOVED_TO"},
        {IN_CREATE, "CREATE"}, {IN_DELETE, "DELETE"}, {IN_DELETE_SELF, "DELETE_SELF"},
        {IN_MOVE_SELF, "MOVE_SELF"}, {IN_UNMOUNT, "UNMOUNT"}, {IN_Q_OVERFLOW, "Q_OVERFLOW"},
        {IN_IGNORED, "IGNORED"}, {IN_ISDIR, "ISDIR"}};
    int first = 1;
    for (unsigned i = 0; i < sizeof t / sizeof t[0]; i++)
        if (m & t[i].b) {
            printf("%s%s", first ? "" : "|", t[i].n);
            first = 0;
            m &= ~t[i].b;
        }
    if (m) printf("|0x%x", m);
}

/* Every queued event, one per line; cookies shown only as same/new. */
static void drain(const char *what, int fd, int wd0) {
    char buf[4096] __attribute__((aligned(8)));
    unsigned last_cookie = 0;
    printf("%s:\n", what);
    for (;;) {
        ssize_t n = read(fd, buf, sizeof buf);
        if (n < 0) {
            if (errno != EAGAIN) printf("  read: %s\n", strerror(errno));
            break;
        }
        for (char *p = buf; p < buf + n;) {
            struct inotify_event *e = (struct inotify_event *)p;
            printf("  wd%+d ", e->wd - wd0);
            bits(e->mask);
            if (e->len) printf(" '%s' len%u", e->name, e->len);
            if (e->cookie) {
                printf(e->cookie == last_cookie ? " cookie=same" : " cookie=new");
                last_cookie = e->cookie;
            }
            printf("\n");
            p += sizeof *e + e->len;
        }
    }
}

static int readable(int fd) {
    struct pollfd p = {fd, POLLIN, 0};
    return poll(&p, 1, 0) == 1 && (p.revents & POLLIN);
}

static int queued(int fd) {
    int n = -1;
    if (ioctl(fd, FIONREAD, &n) < 0) return -errno;
    return n;
}

static void writef(const char *p, const char *s, int flags) {
    int f = open(p, O_WRONLY | flags, 0644);
    if (f < 0) { printf("open %s: %s\n", p, strerror(errno)); return; }
    if (write(f, s, strlen(s)) < 0) printf("write: %s\n", strerror(errno));
    close(f);
}

static void *later(void *arg) {
    usleep(100000);
    writef((const char *)arg, "x", O_CREAT);
    return NULL;
}

#define ERR(call) ((call) < 0 ? strerror(errno) : "ok")

int main(void) {
    char a[256], b[256], sub[256], ln[256], sl[256], f[256];
    path(a, "a"); path(b, "b"); path(sub, "sub"); path(ln, "ln"); path(sl, "sl"); path(f, "f");
    if (mkdir(D, 0755) < 0) { perror("mkdir"); return 1; }
    setvbuf(stdout, NULL, _IOLBF, 0);

    /* 1 */
    int in = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    char c;
    printf("1 init %s cloexec %d nonblock %d\n", in >= 0 ? "ok" : strerror(errno),
           !!(fcntl(in, F_GETFD) & FD_CLOEXEC), !!(fcntl(in, F_GETFL) & O_NONBLOCK));
    printf("1 empty read %s readable %d fionread %d\n", ERR(read(in, &c, 1)), readable(in), queued(in));

    /* 2 */
    unsigned all = IN_MODIFY | IN_ATTRIB | IN_CLOSE_WRITE | IN_MOVED_FROM | IN_MOVED_TO |
                   IN_CREATE | IN_DELETE | IN_DELETE_SELF | IN_MOVE_SELF;
    int dw = inotify_add_watch(in, D, all);
    printf("2 dir watch %s\n", dw > 0 ? "ok" : strerror(errno));
    int fd = open(a, O_CREAT | O_WRONLY, 0644);
    write(fd, "one", 3);
    write(fd, "two", 3);
    close(fd);
    drain("2 create, write twice, close", in, dw);
    chmod(a, 0600);
    drain("2 chmod", in, dw);
    struct timeval tv[2] = {{1000, 0}, {2000, 0}};
    utimes(a, tv);
    drain("2 utimes", in, dw);
    rename(a, b);
    drain("2 rename", in, dw);
    writef(b, "three", O_TRUNC);
    drain("2 open O_TRUNC, write, close", in, dw);
    fd = open(b, O_WRONLY);
    ftruncate(fd, 1);
    close(fd);
    drain("2 ftruncate", in, dw);
    mkdir(sub, 0755);
    rmdir(sub);
    drain("2 mkdir, rmdir", in, dw);
    link(b, ln);
    symlink("b", sl);
    drain("2 link, symlink", in, dw);
    unlink(ln);
    unlink(sl);
    unlink(b);
    drain("2 unlink three", in, dw);

    /* 3 */
    writef(f, "f", O_CREAT);
    int fi = inotify_init1(IN_NONBLOCK);
    int fw = inotify_add_watch(fi, f, IN_ATTRIB | IN_MODIFY | IN_MOVE_SELF | IN_DELETE_SELF);
    chmod(f, 0600);
    writef(f, "g", 0);
    rename(f, a);
    unlink(a);
    drain("3 file: chmod, write, rename, unlink", fi, fw);
    close(fi);

    /* 4 */
    mkdir(sub, 0755);
    int si = inotify_init1(IN_NONBLOCK);
    int sw = inotify_add_watch(si, sub, IN_DELETE_SELF | IN_CREATE);
    rmdir(sub);
    drain("4 watched dir removed", si, sw);
    close(si);
    drain("2 dir saw", in, dw);

    /* 5 */
    writef(f, "f", O_CREAT);
    int again = inotify_add_watch(in, D, IN_CREATE);
    printf("5 same inode same wd %d\n", again == dw);
    printf("5 mask_create on watched %s\n", ERR(inotify_add_watch(in, D, IN_CREATE | IN_MASK_CREATE)));
    printf("5 onlydir on file %s\n", ERR(inotify_add_watch(in, f, IN_CREATE | IN_ONLYDIR)));
    printf("5 no such path %s\n", ERR(inotify_add_watch(in, "inotify.d/none", IN_CREATE)));
    printf("5 no events %s\n", ERR(inotify_add_watch(in, D, 0)));
    inotify_add_watch(in, D, IN_DELETE | IN_MASK_ADD);
    unlink(f);
    writef(f, "f", O_CREAT);
    drain("5 now create|delete only", in, dw);

    /* 6 */
    printf("6 rm_watch %s\n", ERR(inotify_rm_watch(in, dw)));
    drain("6 after rm_watch", in, dw);
    printf("6 rm_watch again %s\n", ERR(inotify_rm_watch(in, dw)));
    unlink(f);
    drain("6 unwatched", in, dw);

    /* 7 */
    int ow = inotify_add_watch(in, D, IN_CREATE | IN_ONESHOT);
    printf("7 oneshot new wd %d\n", ow > dw);
    writef(a, "a", O_CREAT);
    writef(b, "b", O_CREAT);
    drain("7 oneshot, two creates", in, ow);

    /* 8 */
    int pw = inotify_add_watch(in, D, IN_DELETE);
    unlink(a);
    int epfd = epoll_create1(0);
    struct epoll_event ee = {.events = EPOLLIN}, got;
    epoll_ctl(epfd, EPOLL_CTL_ADD, in, &ee);
    printf("8 fionread %d readable %d epoll %d\n", queued(in), readable(in), epoll_wait(epfd, &got, 1, 0));
    char small[sizeof(struct inotify_event)];
    printf("8 short read %s\n", ERR(read(in, small, sizeof small)));
    drain("8 still there", in, pw);
    printf("8 after fionread %d readable %d epoll %d\n", queued(in), readable(in), epoll_wait(epfd, &got, 1, 0));
    close(epfd);
    close(in);

    /* 9 */
    int bi = inotify_init1(0);
    int bw = inotify_add_watch(bi, D, IN_CREATE);
    pthread_t t;
    pthread_create(&t, NULL, later, a);
    char buf[256] __attribute__((aligned(8)));
    ssize_t n = read(bi, buf, sizeof buf);
    struct inotify_event *e = (struct inotify_event *)buf;
    printf("9 blocking read %s wd%+d ", n > 0 ? "ok" : strerror(errno), e->wd - bw);
    bits(e->mask);
    printf(" '%s'\n", e->name);
    pthread_join(t, NULL);
    close(bi);

    /* 10 */
    int ki = inotify_init1(IN_NONBLOCK);
    int kw = inotify_add_watch(ki, D, IN_DELETE);
    unlink(a);
    printf("10 before fork fionread %d\n", queued(ki));
    pid_t pid = fork();
    if (pid == 0) {
        drain("10 child", ki, kw);
        _exit(0);
    }
    int st;
    waitpid(pid, &st, 0);
    printf("10 parent after child fionread %d\n", queued(ki));
    unlink(b);
    drain("10 parent", ki, kw);
    close(ki);

    rmdir(D);
    return 0;
}
