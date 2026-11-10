/* A pty's slave across fork, as a terminal emulator starts its shell: the
 * child makes a session, takes the slave as its controlling terminal, and
 * what it writes there is the parent's to read on the master. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

int main(void) {
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    if (master < 0 || grantpt(master) != 0 || unlockpt(master) != 0) { perror("posix_openpt"); return 1; }
    int slave = open(ptsname(master), O_RDWR | O_NOCTTY);
    if (slave < 0) { perror("open slave"); return 1; }
    struct winsize ws = { .ws_row = 24, .ws_col = 80 };
    printf("parent TIOCSWINSZ: %s\n", ioctl(slave, TIOCSWINSZ, &ws) == 0 ? "ok" : "failed");
    printf("parent isatty(slave): %d\n", isatty(slave));
    fflush(stdout);
    pid_t pid = fork();
    if (pid < 0) { perror("fork"); return 1; }
    if (pid == 0) {
        char line[160];
        int n = 0;
        n += snprintf(line + n, sizeof line - n, "child isatty: %d, ", isatty(slave));
        n += snprintf(line + n, sizeof line - n, "setsid: %s, ", setsid() >= 0 ? "ok" : "failed");
        n += snprintf(line + n, sizeof line - n, "TIOCSCTTY: %s\n", ioctl(slave, TIOCSCTTY, 0) == 0 ? "ok" : "failed");
        write(slave, line, strlen(line));
        _exit(0);
    }
    close(slave);
    int status = 0;
    waitpid(pid, &status, 0);
    char buf[256] = {0};
    int got = 0;
    for (int tries = 0; tries < 50 && got < (int)sizeof buf - 1; tries++) {
        int r = read(master, buf + got, sizeof buf - 1 - got);
        if (r <= 0) break;
        got += r;
        if (strchr(buf, '\n')) break;
    }
    /* The line discipline turns "\n" into "\r\n". */
    for (char *p = buf; *p; p++) if (*p == '\r') memmove(p, p + 1, strlen(p));
    printf("master read: %s", got > 0 ? buf : "nothing\n");
    printf("child exit %d\n", WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    return 0;
}
