/* memfd_create's and eventfd2's flags, one line per check, compared with
 * native: every flag bit set (how Chromium's Mojo probes for both) and an
 * unknown bit are EINVAL; the flags each defines are accepted. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

static void check(const char *what, unsigned flags) {
    errno = 0;
    long r = syscall(SYS_memfd_create, "", flags);
    printf("memfd_create %s: %s\n", what, r >= 0 ? "ok" : strerror(errno));
    if (r >= 0) close((int)r);
}

static void echeck(const char *what, unsigned flags) {
    errno = 0;
    long r = syscall(SYS_eventfd2, 0, flags);
    printf("eventfd2 %s: %s\n", what, r >= 0 ? "ok" : strerror(errno));
    if (r >= 0) close((int)r);
}

int main(void) {
    check("every bit", ~0u);
    check("unknown bit 0x100", 0x100);
    check("cloexec", MFD_CLOEXEC);
    check("cloexec allow-sealing", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    echeck("every bit", ~0u);
    echeck("unknown bit 0x100", 0x100);
    echeck("cloexec nonblock semaphore", EFD_CLOEXEC | EFD_NONBLOCK | EFD_SEMAPHORE);
    return 0;
}
