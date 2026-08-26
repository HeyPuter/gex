/*
 * The pseudoterminal dance, step by step, so an empty terminal emulator can be
 * told apart from a broken pty.
 *
 * A terminal emulator opens `/dev/ptmx`, unlocks it, forks, and reads what the
 * child writes to the slave. In gex the pair lives in the global kernel and the
 * two ends are two Workers, so every step here crosses a boundary that a
 * single-process test would not. Each one says what happened, because "xterm is
 * blank" is consistent with any of them failing.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

int main(void) {
    int m = open("/dev/ptmx", O_RDWR | O_NOCTTY);
    printf("ptmx open: %d (%s)\n", m, m < 0 ? strerror(errno) : "ok");
    if (m < 0) return 1;

    int ptn = -1;
    if (ioctl(m, TIOCGPTN, &ptn) < 0) {
        printf("TIOCGPTN: %s\n", strerror(errno));
        return 1;
    }
    printf("TIOCGPTN: /dev/pts/%d\n", ptn);

    int unlock = 0;
    if (ioctl(m, TIOCSPTLCK, &unlock) < 0) {
        printf("TIOCSPTLCK: %s\n", strerror(errno));
    } else {
        printf("TIOCSPTLCK: unlocked\n");
    }

    char slave[64];
    snprintf(slave, sizeof slave, "/dev/pts/%d", ptn);

    pid_t pid = fork();
    if (pid < 0) {
        printf("fork: %s\n", strerror(errno));
        return 1;
    }
    if (pid == 0) {
        /* The child is what a shell would be: its own session, the slave as
         * its controlling terminal, and a line of output. */
        if (setsid() < 0) {
            printf("child setsid: %s\n", strerror(errno));
        }
        int s = open(slave, O_RDWR);
        if (s < 0) {
            printf("child slave open: %s\n", strerror(errno));
            _exit(1);
        }
        printf("child opened %s\n", slave);
        if (ioctl(s, TIOCSCTTY, 0) < 0) {
            printf("child TIOCSCTTY: %s\n", strerror(errno));
        }
        dup2(s, 1);
        const char *line = "SLAVE-WROTE-THIS\n";
        ssize_t w = write(1, line, strlen(line));
        if (w < 0) {
            dup2(2, 1);
            printf("child write: %s\n", strerror(errno));
        }
        _exit(0);
    }

    /* The parent is the terminal emulator: read the master. */
    char buf[256];
    ssize_t n = -1;
    for (int tries = 0; tries < 200; tries++) {
        n = read(m, buf, sizeof buf - 1);
        if (n > 0) break;
        if (n < 0 && errno != EAGAIN && errno != EINTR) break;
        usleep(50000);
    }
    if (n > 0) {
        buf[n] = 0;
        for (char *p = buf; *p; p++) {
            if (*p == '\r' || *p == '\n') *p = '.';
        }
        printf("master read %zd bytes: %s\n", n, buf);
    } else {
        printf("master read: %zd (%s) -- nothing came through\n", n,
               n < 0 ? strerror(errno) : "empty");
    }
    int status = 0;
    waitpid(pid, &status, 0);
    printf("child exited %d\n", WEXITSTATUS(status));
    return n > 0 ? 0 : 1;
}
