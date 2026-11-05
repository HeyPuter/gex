/* A terminal emulator's pty, end to end: posix_openpt, grantpt, unlockpt,
 * ptsname; a child that makes the slave its controlling terminal (setsid,
 * TIOCSCTTY) and its stdin/stdout/stderr, says hello, reads a line and
 * echoes it back; the parent reads the master, types the line once the hello
 * is there (the line discipline echoes it), and reads to the hangup (EIO)
 * after the child exits. What the master saw is printed with \r shown, so
 * ONLCR and the echo are checked too. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <unistd.h>

static char seen[4096];
static size_t got;

/* Read the master until `want` has been seen, end of file/EIO, or a cap. */
static int until(int m, const char *want) {
    while (got < sizeof seen - 1) {
        seen[got] = 0;
        if (want && strstr(seen, want)) return 1;
        ssize_t r = read(m, seen + got, sizeof seen - 1 - got);
        if (r <= 0) return r < 0 && errno == EIO ? 2 : 0;
        got += r;
    }
    return 0;
}

int main(void) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0) { printf("posix_openpt: %s\n", strerror(errno)); return 1; }
    if (grantpt(m) || unlockpt(m)) { printf("grantpt/unlockpt: %s\n", strerror(errno)); return 1; }
    const char *name = ptsname(m);
    printf("ptsname %s\n", name && !strncmp(name, "/dev/pts/", 9) ? "/dev/pts/N" : name ? name : "(none)");
    struct winsize ws = { 24, 80, 0, 0 };
    ioctl(m, TIOCSWINSZ, &ws);
    fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) {
        close(m);
        setsid();
        int s = open(name, O_RDWR);
        if (s < 0) _exit(10);
        if (ioctl(s, TIOCSCTTY, 0)) _exit(11);
        dup2(s, 0); dup2(s, 1); dup2(s, 2);
        if (s > 2) close(s);
        struct winsize w;
        ioctl(0, TIOCGWINSZ, &w);
        char *tn = ttyname(0);
        printf("hello-pty isatty=%d tty=%s %dx%d pgrp-is-fg=%d\n", isatty(0),
               tn && !strncmp(tn, "/dev/pts/", 9) ? "/dev/pts/N" : tn ? tn : "(none)",
               w.ws_col, w.ws_row, tcgetpgrp(0) == getpgrp());
        fflush(stdout);
        char line[64];
        if (!fgets(line, sizeof line, stdin)) _exit(12);
        line[strcspn(line, "\n")] = 0;
        printf("got:%s\n", line);
        fflush(stdout);
        _exit(0);
    }
    if (!until(m, "hello-pty")) printf("no hello\n");
    if (!until(m, "\n")) printf("no end of hello\n");
    write(m, "abc\n", 4);
    int end = until(m, NULL);
    int st = 0;
    waitpid(pid, &st, 0);
    seen[got] = 0;
    printf("master saw:\n");
    for (char *p = seen; *p; p++) {
        if (*p == '\r') fputs("\\r", stdout);
        else putchar(*p);
    }
    printf("end %s, child status %d\n", end == 2 ? "EIO" : end == 0 ? "eof/cap" : "?", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    return 0;
}
