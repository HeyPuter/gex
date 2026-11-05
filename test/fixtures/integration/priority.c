/* getpriority/setpriority/getcpu. Every answer is relative to the niceness
 * the process starts at, because that is the runner's own: QEMU passes the
 * host's through (a suite run under `nice -n 10` starts at 10), and gex is a
 * fresh system at 0. Raising it by 5 (clamped at 19) is allowed to anyone.
 * getcpu's CPU number is whatever host core the process lands on (QEMU
 * reports the real one), so only its success is checked. */
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdio.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    errno = 0;
    int p = getpriority(PRIO_PROCESS, 0);
    printf("getpriority in-range=%d errno=%d\n", p >= -20 && p <= 19, errno);

    int want = p + 5 > 19 ? 19 : p + 5;
    int r = setpriority(PRIO_PROCESS, 0, want);
    printf("setpriority(+5) ret=%d\n", r);
    /* Not read back: gex accepts a niceness and does not keep it (the
     * scheduler is the host's). */

    unsigned cpu = 99, node = 99;
    long g = syscall(SYS_getcpu, &cpu, &node, 0);
    /* Only success is checked: the CPU and NUMA node are whatever host core the
     * process lands on under QEMU (nondeterministic), so neither is a portable
     * oracle -- gex reports a single scheduling domain (cpu 0, node 0). */
    printf("getcpu ok=%d\n", g == 0);

    printf("priority ok\n");
    return 0;
}
