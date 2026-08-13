/* getpriority/setpriority/getcpu: we run at the default niceness in a single
 * scheduling domain. getpriority and setpriority are deterministic and so are
 * checked against QEMU directly; getcpu's CPU number is whatever host core the
 * process lands on (QEMU reports the real one), so only its success is checked. */
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
    printf("getpriority=%d errno=%d\n", p, errno);

    int r = setpriority(PRIO_PROCESS, 0, 5);
    printf("setpriority ret=%d\n", r);

    unsigned cpu = 99, node = 99;
    long g = syscall(SYS_getcpu, &cpu, &node, 0);
    /* Only success is checked: the CPU and NUMA node are whatever host core the
     * process lands on under QEMU (nondeterministic), so neither is a portable
     * oracle -- gex reports a single scheduling domain (cpu 0, node 0). */
    printf("getcpu ok=%d\n", g == 0);

    printf("priority ok\n");
    return 0;
}
