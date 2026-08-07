/* Three things a Linux program assumes exist, and gex did not have.
 *
 * All three turned up trying to run Xwayland, one after the other, each
 * hiding the next.
 *
 * `#!` interpreter scripts. Linux resolves these in `execve`, not in the
 * shell: the kernel reads the first line and runs the named interpreter with
 * the script's path inserted as an argument. Without it, Debian's
 * `x11-common` postinst ran a Perl script through debconf, dash got ENOENT,
 * fell back to reading the file as shell, and reported `use: not found`.
 *
 * `flock`. Missing entirely, so perl's `flock` returned ENOSYS and debconf
 * read that as "locked by another process".
 *
 * `epoll`. Missing entirely, so Xwayland stopped with "failed to allocate
 * poll structure" before printing anything else.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static void write_script(const char *path, const char *body) {
    FILE *f = fopen(path, "w");
    if (!f) {
        perror("fopen");
        return;
    }
    fputs(body, f);
    fclose(f);
    chmod(path, 0755);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    /* The interpreter half: print what the kernel passed and stop. */
    /* `strncmp`, not `strcmp`: the second script's `#!` line is
       `--interp with two words`, and Linux passes all of that as a *single*
       argument. Comparing the whole string missed it, the child fell through
       to the top of the test, and it re-ran itself forever -- which is a
       tidy demonstration of the rule. */
    if (argc > 1 && strncmp(argv[1], "--interp", 8) == 0) {
        printf("interp arg1 [%s]\n", argv[1]);
        printf("interp got:");
        for (int i = 2; i < argc; i++) {
            /* The script's own path varies with where the test was built, so
               only its last component is printed. */
            const char *a = argv[i];
            const char *slash = strrchr(a, '/');
            printf(" [%s]", slash ? slash + 1 : a);
        }
        printf("\n");
        return 4;
    }

    /* -- shebang ------------------------------------------------------- */

    /* The interpreter is this program itself, so the test needs nothing in
       the filesystem but the scripts. Run with `--interp` it prints the
       arguments the kernel handed it, which is the whole question: Linux
       inserts the script's path, and everything after the first space on the
       `#!` line is a single argument however many spaces it contains. */
    char me[4096];
    ssize_t len = readlink("/proc/self/exe", me, sizeof me - 1);
    if (len <= 0) {
        printf("no /proc/self/exe\n");
        return 1;
    }
    me[len] = 0;

    char body[8192];
    snprintf(body, sizeof body, "#!%s --interp\n", me);
    write_script("/tmp/shebang1", body);
    snprintf(body, sizeof body, "#!%s --interp with two words\n", me);
    write_script("/tmp/shebang2", body);
    write_script("/tmp/shebang3", "not a shebang at all\n");

    for (int i = 1; i <= 3; i++) {
        char path[64];
        snprintf(path, sizeof path, "/tmp/shebang%d", i);
        pid_t p = fork();
        if (p == 0) {
            char *argv[] = {path, "extra", NULL};
            execv(path, argv);
            printf("exec %d failed: %s\n", i,
                   errno == ENOEXEC ? "ENOEXEC" : strerror(errno));
            _exit(9);
        }
        int st = 0;
        waitpid(p, &st, 0);
        printf("script %d exited %d\n", i, WEXITSTATUS(st));
    }

    /* -- flock --------------------------------------------------------- */

    int a = open("/tmp/gex-lockfile", O_CREAT | O_RDWR, 0644);
    int b = open("/tmp/gex-lockfile", O_RDWR);
    int other = open("/tmp/gex-otherfile", O_CREAT | O_RDWR, 0644);
    printf("exclusive: %d\n", flock(a, LOCK_EX | LOCK_NB));
    /* A second open of the same file is a different description, so it
       contends. */
    printf("second on the same file: %d %s\n", flock(b, LOCK_EX | LOCK_NB),
           errno == EWOULDBLOCK ? "EWOULDBLOCK" : "?");
    /* A different file does not. This is the one that was wrong: with the
       conflict test written over owners alone, locking anything locked
       everything. */
    printf("a different file: %d\n", flock(other, LOCK_EX | LOCK_NB));
    /* A duplicate shares the description, so it may lock again. */
    int dup_a = dup(a);
    printf("through a dup: %d\n", flock(dup_a, LOCK_EX | LOCK_NB));
    printf("unlock: %d\n", flock(a, LOCK_UN));
    printf("after unlock: %d\n", flock(b, LOCK_EX | LOCK_NB));
    /* Shared locks coexist. */
    flock(b, LOCK_UN);
    int c = open("/tmp/gex-lockfile", O_RDWR);
    printf("shared then shared: %d %d\n", flock(b, LOCK_SH | LOCK_NB),
           flock(c, LOCK_SH | LOCK_NB));
    flock(b, LOCK_UN);
    flock(c, LOCK_UN);
    close(a);
    close(b);
    close(c);
    close(dup_a);
    close(other);
    unlink("/tmp/gex-lockfile");
    unlink("/tmp/gex-otherfile");

    /* -- epoll --------------------------------------------------------- */

    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (ep < 0) {
        printf("epoll_create1: %s\n", strerror(errno));
        return 1;
    }
    int fds[2];
    if (pipe(fds) != 0) {
        perror("pipe");
        return 1;
    }
    struct epoll_event ev;
    memset(&ev, 0, sizeof ev);
    ev.events = EPOLLIN;
    ev.data.u64 = 0xfeedfaceUL;
    printf("add: %d\n", epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &ev));
    printf("add twice: %d %s\n", epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &ev),
           errno == EEXIST ? "EEXIST" : "?");

    struct epoll_event out[4];
    printf("nothing ready: %d\n", epoll_wait(ep, out, 4, 0));
    write(fds[1], "x", 1);
    int n = epoll_wait(ep, out, 4, 0);
    printf("after a write: %d events=%s data=%llx\n", n,
           n == 1 && (out[0].events & EPOLLIN) ? "EPOLLIN" : "?",
           n == 1 ? (unsigned long long)out[0].data.u64 : 0ULL);

    /* Modifying replaces the token, which is how a program that reuses a
       descriptor finds the right state. */
    ev.data.u64 = 0x1234;
    printf("mod: %d\n", epoll_ctl(ep, EPOLL_CTL_MOD, fds[0], &ev));
    n = epoll_wait(ep, out, 4, 0);
    printf("after mod: %d data=%llx\n", n,
           n == 1 ? (unsigned long long)out[0].data.u64 : 0ULL);

    printf("del: %d\n", epoll_ctl(ep, EPOLL_CTL_DEL, fds[0], &ev));
    printf("del twice: %d %s\n", epoll_ctl(ep, EPOLL_CTL_DEL, fds[0], &ev),
           errno == ENOENT ? "ENOENT" : "?");
    printf("after del: %d\n", epoll_wait(ep, out, 4, 0));
    close(fds[0]);
    close(fds[1]);
    close(ep);
    return 0;
}
