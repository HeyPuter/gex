/*
 * A pty in NON-canonical mode, which is what a line editor puts it in.
 *
 * `pty-exec` covers the shape a terminal emulator uses with a shell that reads
 * whole lines: canonical mode, where the line discipline echoes what is typed
 * and hands the reader a line at a time. A shell with a line editor -- bash
 * with readline, and anything else that draws its own prompt -- turns ICANON
 * and ECHO off and reads a byte at a time, waiting between them. That is a
 * different path through the terminal, and it is the one that broke: an
 * interactive bash echoed the first character typed at it and then went deaf,
 * while the same bash with `--noediting` (which leaves the terminal canonical)
 * typed fine.
 *
 * Two things could do that, so the child can wait either way. With `select` it
 * waits to be told a byte is there; with `read` it just blocks. If the first
 * stops after one byte and the second does not, readiness is what is wrong,
 * not delivery.
 *
 *   pty-raw [select|read]        (default: select)
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>

/* The keystrokes, one write each -- a terminal emulator never sends two at
 * once, and a bug that only shows up between two writes is invisible when the
 * whole line arrives in one. */
static const char KEYS[] = "abcq";

int main(int argc, char **argv) {
    int use_select = !(argc > 1 && !strcmp(argv[1], "read"));
    /* A third way to wait, which reports rather than waits: see below. */
    int watch = argc > 1 && !strcmp(argv[1], "watch");
    printf("pty-raw: the child waits in %s\n",
           watch ? "select, reporting what the terminal has queued"
                 : use_select ? "select" : "read");
    fflush(stdout);

    int m = open("/dev/ptmx", O_RDWR | O_NOCTTY);
    if (m < 0) { printf("ptmx: %s\n", strerror(errno)); return 1; }
    int ptn = -1, unlock = 0;
    if (ioctl(m, TIOCGPTN, &ptn) < 0) { printf("TIOCGPTN: %s\n", strerror(errno)); return 1; }
    ioctl(m, TIOCSPTLCK, &unlock);
    char slave[64];
    snprintf(slave, sizeof slave, "/dev/pts/%d", ptn);

    pid_t pid = fork();
    if (pid < 0) { printf("fork: %s\n", strerror(errno)); return 1; }
    if (pid == 0) {
        setsid();
        int s = open(slave, O_RDWR);
        if (s < 0) _exit(127);
        ioctl(s, TIOCSCTTY, 0);
        dup2(s, 0); dup2(s, 1); dup2(s, 2);
        if (s > 2) close(s);
        close(m);

        /* Exactly what readline does to the terminal before it reads: no line
         * editing by the kernel, no echo by the kernel, one byte is enough. */
        struct termios t;
        if (tcgetattr(0, &t) < 0) _exit(126);
        t.c_lflag &= ~(ICANON | ECHO);
        t.c_cc[VMIN] = 1;
        t.c_cc[VTIME] = 0;
        if (tcsetattr(0, TCSANOW, &t) < 0) _exit(125);
        /* Say it landed, and say what came back -- a tcsetattr the terminal
         * accepts and then ignores would otherwise look like a delivery bug. */
        struct termios back;
        tcgetattr(0, &back);
        char note[96];
        int k = snprintf(note, sizeof note, "child: raw mode set, ICANON=%d ECHO=%d\n",
                         (back.c_lflag & ICANON) != 0, (back.c_lflag & ECHO) != 0);
        ssize_t ignored = write(1, note, k);
        (void)ignored;

        for (;;) {
            if (watch) {
                /* The question the other two modes cannot answer: is the byte
                 * there and the reader not being told, or is it not there?
                 * `FIONREAD` is what the terminal itself says is queued for
                 * this end, so a round that says `queued 1` while `select`
                 * reported nothing is a readiness bug, and a round that says
                 * `queued 0` after a keystroke is a delivery bug. */
                for (int round = 0; round < 6; round++) {
                    fd_set rs;
                    FD_ZERO(&rs);
                    FD_SET(0, &rs);
                    struct timeval tv = { .tv_sec = 0, .tv_usec = 200000 };
                    int r = select(1, &rs, NULL, NULL, &tv);
                    int q = -1;
                    ioctl(0, FIONREAD, &q);
                    k = snprintf(note, sizeof note, "child: select %d, queued %d\n", r, q);
                    ignored = write(1, note, k);
                    if (r > 0) break;
                }
            }
            if (use_select && !watch) {
                fd_set rs;
                FD_ZERO(&rs);
                FD_SET(0, &rs);
                /* No timeout: this is the call readline blocks in, and a wait
                 * that never ends is the symptom being chased. */
                int r = select(1, &rs, NULL, NULL, NULL);
                if (r < 0) {
                    k = snprintf(note, sizeof note, "child: select failed: %s\n", strerror(errno));
                    ignored = write(1, note, k);
                    _exit(1);
                }
            }
            char c;
            ssize_t n = read(0, &c, 1);
            if (n <= 0) {
                k = snprintf(note, sizeof note, "child: read returned %zd (%s)\n",
                             n, n < 0 ? strerror(errno) : "eof");
                ignored = write(1, note, k);
                _exit(n < 0 ? 1 : 0);
            }
            k = snprintf(note, sizeof note, "child got [%c]\n", c);
            ignored = write(1, note, k);
            if (c == 'q') _exit(0);
        }
    }

    /* The terminal emulator's side: type a key, then read whatever the child
     * says about it before typing the next one. */
    for (unsigned i = 0; i < sizeof KEYS - 1; i++) {
        usleep(400000);
        ssize_t w = write(m, &KEYS[i], 1);
        printf("typed '%c' (%zd byte)\n", KEYS[i], w);
        fflush(stdout);
        /* Half a second of listening: enough for a working terminal to answer
         * and short enough that four keys fit in one run. */
        for (int round = 0; round < 5; round++) {
            struct pollfd p = { .fd = m, .events = POLLIN };
            if (poll(&p, 1, 100) <= 0) continue;
            char buf[256];
            ssize_t n = read(m, buf, sizeof buf - 1);
            if (n <= 0) break;
            buf[n] = 0;
            for (ssize_t j = 0; j < n; j++)
                if (buf[j] == '\r') buf[j] = ' ';
            printf("master read: %s", buf);
            fflush(stdout);
        }
    }

    int status = 0;
    for (int i = 0; i < 20 && waitpid(pid, &status, WNOHANG) != pid; i++) usleep(100000);
    printf("pty-raw: done\n");
    fflush(stdout);
    return 0;
}
