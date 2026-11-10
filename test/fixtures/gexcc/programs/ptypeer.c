/* TIOCGPTPEER: a pty's slave opened from its master, as rustix's openpty
 * (and so alacritty and every Rust terminal on it) does, with no path. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#ifndef TIOCGPTPEER
#define TIOCGPTPEER 0x5441
#endif

int main(void) {
    int master = posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC);
    if (master < 0 || unlockpt(master) != 0) { perror("posix_openpt"); return 1; }
    int slave = ioctl(master, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    printf("TIOCGPTPEER: %s\n", slave >= 0 ? "ok" : strerror(errno));
    if (slave < 0) return 0;
    printf("isatty(slave): %d\n", isatty(slave));
    printf("slave is the master's: %s\n", write(slave, "x\n", 2) == 2 ? "written" : strerror(errno));
    char buf[8] = {0};
    int n = read(master, buf, sizeof buf - 1);
    printf("master read %d bytes\n", n);
    return 0;
}
