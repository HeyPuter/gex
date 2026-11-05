// A terminal's "polling read" (termios(3)): in non-canonical mode with VMIN 0
// and VTIME 0 a blocking read with nothing to read returns 0 at once; a
// non-blocking one still gets EAGAIN; with a byte typed the read takes it.
// btop reads its keys so, until a read gives 0: gex parked that read, and
// btop froze on its first key. Prints one line per check.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    alarm(20); // a parked read fails the test instead of hanging it
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0 || grantpt(m) || unlockpt(m)) { perror("posix_openpt"); return 1; }
    int s = open(ptsname(m), O_RDWR | O_NOCTTY);
    if (s < 0) { perror("open slave"); return 1; }
    struct termios t; tcgetattr(s, &t);
    t.c_lflag &= ~(ICANON | ECHO); t.c_cc[VMIN] = 0; t.c_cc[VTIME] = 0;
    tcsetattr(s, TCSANOW, &t);
    char c = 0; double t0 = now();
    ssize_t n = read(s, &c, 1);
    printf("empty: %zd%s\n", n, now() - t0 < 1 ? "" : " (waited)");
    write(m, "q", 1);
    usleep(100000);
    n = read(s, &c, 1);
    printf("typed: %zd '%c'\n", n, n == 1 ? c : '?');
    n = read(s, &c, 1);
    printf("after: %zd\n", n);
    fcntl(s, F_SETFL, O_NONBLOCK);
    n = read(s, &c, 1);
    printf("nonblocking: %zd %s\n", n, n < 0 && errno == EAGAIN ? "EAGAIN" : "?");
    return 0;
}
