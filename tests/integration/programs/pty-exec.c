/*
 * A terminal emulator's own sequence: a pty pair, a shell EXEC'd on the slave,
 * and a poll loop on the master. `pty-pair` proves the pair works when one
 * process holds both ends; this proves it for the shape xterm actually uses,
 * where the reader waits in `poll`/`select` and the writer is a different
 * program in a different Worker.
 *
 * Every step prints, because "the terminal is blank" is consistent with the
 * child never starting, the slave closing under it, or readiness never being
 * reported for data that did arrive.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

static void show(const char *what, const char *buf, ssize_t n) {
    char out[512];
    ssize_t k = 0;
    for (ssize_t i = 0; i < n && k < (ssize_t)sizeof out - 4; i++) {
        unsigned char c = (unsigned char)buf[i];
        if (c == '\r' || c == '\n') out[k++] = '.';
        else if (c < 32 || c > 126) k += snprintf(out + k, sizeof out - k, "\\%o", c);
        else out[k++] = c;
    }
    out[k] = 0;
    printf("%s %zd bytes: %s\n", what, n, out);
}

int main(int argc, char **argv) {
    const char *cmd = argc > 1 ? argv[1] : "echo HELLO-FROM-EXEC";

    int m = open("/dev/ptmx", O_RDWR | O_NOCTTY);
    if (m < 0) { printf("ptmx: %s\n", strerror(errno)); return 1; }
    int ptn = -1, unlock = 0;
    if (ioctl(m, TIOCGPTN, &ptn) < 0) { printf("TIOCGPTN: %s\n", strerror(errno)); return 1; }
    ioctl(m, TIOCSPTLCK, &unlock);
    char slave[64];
    snprintf(slave, sizeof slave, "/dev/pts/%d", ptn);
    printf("pty: master fd %d, slave %s\n", m, slave);

    /* xterm opens the slave in the parent too, sets its modes, and closes it
     * after the fork. That close must not take the child's end with it. */
    int pre = open(slave, O_RDWR | O_NOCTTY);
    printf("parent slave open: %d (%s)\n", pre, pre < 0 ? strerror(errno) : "ok");

    pid_t pid = fork();
    if (pid < 0) { printf("fork: %s\n", strerror(errno)); return 1; }
    if (pid == 0) {
        setsid();
        int s = open(slave, O_RDWR);
        if (s < 0) { printf("child slave open: %s\n", strerror(errno)); _exit(127); }
        ioctl(s, TIOCSCTTY, 0);
        dup2(s, 0); dup2(s, 1); dup2(s, 2);
        if (s > 2) close(s);
        close(m);
        if (pre > 2) close(pre);
        /* A path runs directly; anything else goes through the shell. Direct
         * matters for a run with nothing but this program bundled -- the
         * shape being tested (a pty, a session, a child that forks) needs no
         * shell in it. */
        if (cmd[0] == '/')
            execl(cmd, cmd, (char *)NULL);
        else
            execl("/bin/sh", "sh", "-c", cmd, (char *)NULL);
        /* Still here means exec failed, and stderr is the slave now. */
        char why[128];
        int n = snprintf(why, sizeof why, "EXEC-FAILED %s\n", strerror(errno));
        ssize_t ignored = write(2, why, n);
        (void)ignored;
        _exit(126);
    }
    if (pre >= 0) close(pre);   /* exactly what xterm does */
    printf("forked child %d, parent closed its slave\n", pid);

    /* Anything on the command line after the program is typed at the terminal:
     * written to the master, so the line discipline echoes it and the child
     * reads it as input. This is how a shell reading commands from a terminal
     * is tested without a terminal emulator in the way. */
    if (argc > 2) {
        usleep(400000);
        char line[256];
        int n = snprintf(line, sizeof line, "%s\n", argv[2]);
        ssize_t w = write(m, line, n);
        printf("typed %zd bytes: %s", w, line);
        fflush(stdout);
    }

    int reads = 0;
    /* Long enough to see a second and third write, which is the point: a
     * terminal that prints one line and stops looks fine for the first
     * quarter-second. */
    for (int round = 0; round < 200; round++) {
        struct pollfd p = { .fd = m, .events = POLLIN };
        int r = poll(&p, 1, 250);
        if (r > 0 && (p.revents & POLLIN)) {
            char buf[256];
            ssize_t n = read(m, buf, sizeof buf);
            if (n > 0) { show("poll said ready, read", buf, n); reads++; }
            else { printf("poll said ready, read returned %zd (%s)\n", n, n < 0 ? strerror(errno) : "eof"); break; }
        } else if (r > 0) {
            printf("poll: revents %#x (hup/err, not data)\n", p.revents);
            break;
        }

    }
    if (!reads) {
        /* Was it readiness that failed, or the data? A blocking read says. */
        int fl = fcntl(m, F_GETFL, 0);
        fcntl(m, F_SETFL, fl & ~O_NONBLOCK);
        char buf[256];
        ssize_t n = read(m, buf, sizeof buf);
        if (n > 0) show("poll never fired, but a blocking read got", buf, n);
        else printf("blocking read: %zd (%s) -- no output at all\n", n, n < 0 ? strerror(errno) : "eof");
    }
    /* And select(), which is what xterm's main loop really calls. */
    fd_set rs;
    FD_ZERO(&rs); FD_SET(m, &rs);
    struct timeval tv = { .tv_sec = 0, .tv_usec = 200000 };
    int sr = select(m + 1, &rs, NULL, NULL, &tv);
    printf("select after drain: %d (%s)\n", sr, sr < 0 ? strerror(errno) : (FD_ISSET(m, &rs) ? "readable" : "idle"));

    int status = 0;
    if (waitpid(pid, &status, WNOHANG) == pid) printf("child exited %d\n", WEXITSTATUS(status));
    else printf("child still running\n");
    return reads ? 0 : 1;
}
