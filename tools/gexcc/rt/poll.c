/* gexcc runtime: a loop's safepoint (poll/GexPoll.cpp). Re-arm the
 * countdown and make one system call: gex delivers pending signals at a
 * system call's return and checks the interval timers there, so a loop that
 * never calls the kernel still sees alarm(), SIGINT and SIGTERM. */
#include <unistd.h>
#include <sys/syscall.h>

void __gex_poll_rearm(int);

/* back-edges between polls: about 65k loop iterations */
#define GEX_POLL_INTERVAL 65536

__attribute__((cold, noinline)) void __gex_poll(void)
{
	__gex_poll_rearm(GEX_POLL_INTERVAL);
	syscall(SYS_getpid);
}
