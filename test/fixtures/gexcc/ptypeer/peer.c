/* TIOCGPTPEER: a master opens its own slave without the slave's name, with
 * the flags given (O_CLOEXEC kept, others outside the open flags dropped).
 * The slave it gives is the one ptsname names, and bytes go both ways through
 * the pair. Asked of the slave, or of a file that is no terminal, it is
 * refused, as Linux refuses it. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <termios.h>
#include <unistd.h>

#ifndef TIOCGPTPEER
#define TIOCGPTPEER 0x5441
#endif

int main(void) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0 || grantpt(m) || unlockpt(m)) { printf("openpt: %s\n", strerror(errno)); return 1; }
    int s = ioctl(m, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (s < 0) { printf("peer: %s\n", strerror(errno)); return 1; }
    const char *name = ptsname(m);
    struct stat a, b;
    int same = name && !stat(name, &a) && !fstat(s, &b) && a.st_rdev == b.st_rdev;
    char *tn = ttyname(s);
    printf("peer isatty=%d same=%d ttyname=%d cloexec=%d\n", isatty(s), same,
           tn && name && !strcmp(tn, name), (fcntl(s, F_GETFD) & FD_CLOEXEC) != 0);

    struct termios t;
    tcgetattr(s, &t);
    cfmakeraw(&t);
    tcsetattr(s, TCSANOW, &t);
    char buf[32] = {0};
    write(s, "up", 2);
    ssize_t r = read(m, buf, sizeof buf - 1);
    printf("slave->master %.*s\n", (int)(r > 0 ? r : 0), buf);
    write(m, "down", 4);
    r = read(s, buf, sizeof buf - 1);
    printf("master->slave %.*s\n", (int)(r > 0 ? r : 0), buf);

    errno = 0;
    int bad = ioctl(s, TIOCGPTPEER, O_RDWR);
    printf("on slave %d %s\n", bad, bad < 0 && errno == ENOTTY ? "ENOTTY" : strerror(errno));
    int nul = open("/dev/null", O_RDWR);
    errno = 0;
    bad = ioctl(nul, TIOCGPTPEER, O_RDWR);
    printf("on /dev/null %d %s\n", bad, bad < 0 && errno == ENOTTY ? "ENOTTY" : strerror(errno));
    return 0;
}
