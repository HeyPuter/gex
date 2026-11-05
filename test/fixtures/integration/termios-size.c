/* How much of `struct termios` does TCGETS write? It must be
 * `sizeof(struct __kernel_termios)` -- 36 bytes here -- not the larger
 * structure a C library declares. Writing the userspace shape overruns glibc's
 * kernel-sized stack buffer onto its canary. Checked the way glibc does it: a
 * kernel-sized buffer with a known guard pattern after it. */
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define TCGETS 0x5401
#define KERNEL_TERMIOS 36
#define GUARD 32

int main(void) {
    if (!isatty(0)) {
        /* Nothing to ask. Said out loud so a no-op run is not read as a pass. */
        printf("not a tty\n");
        return 0;
    }

    struct {
        unsigned char t[KERNEL_TERMIOS];
        unsigned char guard[GUARD];
    } buf;
    memset(&buf, 0, sizeof buf);
    memset(buf.guard, 0xA5, GUARD);

    if (ioctl(0, TCGETS, buf.t) != 0) {
        printf("TCGETS failed\n");
        return 1;
    }

    int clobbered = 0;
    for (int i = 0; i < GUARD; i++) {
        if (buf.guard[i] != 0xA5) clobbered++;
    }
    printf("clobbered %d\n", clobbered);

    /* The control characters matter too: a shell reads its erase and
       interrupt keys from here, and all-zero c_cc means backspace does
       nothing. c_cc starts after the four flag words and c_line. */
    const unsigned char *cc = buf.t + 17;
    printf("VINTR %d VERASE %d VKILL %d VEOF %d\n", cc[0], cc[2], cc[3], cc[4]);
    return 0;
}
