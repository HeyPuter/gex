/* How much of `struct termios` does TCGETS write?
 *
 * The answer has to be `sizeof(struct __kernel_termios)` -- four flag words,
 * c_line, and c_cc[19], so 36 bytes on this architecture -- and not the
 * larger structure a C library declares for its own use, which has c_cc[32]
 * and two speed fields.
 *
 * This is not a detail. glibc's tcgetattr puts a kernel-sized structure on
 * the caller's stack and passes that; a kernel writing the userspace shape
 * into it overruns by 24 bytes and lands on the stack canary. `bash -i` died
 * that way, reporting stack smashing before it had printed a prompt, while
 * every musl program was fine -- musl passes its own larger structure, so the
 * overrun landed on its own padding.
 *
 * So the check is deliberately made the way glibc makes it: a buffer of
 * exactly the kernel's size, with a known pattern after it, and a look at
 * whether the pattern survived.
 */
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define TCGETS 0x5401
#define KERNEL_TERMIOS 36
#define GUARD 32

int main(void) {
    if (!isatty(0)) {
        /* Nothing to ask. Said out loud so a run that silently checked
           nothing cannot be mistaken for a pass. */
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
