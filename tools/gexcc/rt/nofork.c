/* gexcc runtime: a link that never forks (-fgex-no-fork, GEXCC_NOFORK=1).
 *
 * fork on wasm32 unwinds and rewinds the wasm stack, so a link that can
 * reach it is instrumented with asyncify wherever fork might be below a
 * frame. In a shared-mode link that is every function making an indirect
 * call, which in a large C++ program is nearly all of it. A program that
 * starts its children with posix_spawn (or not at all) can opt out: the
 * driver links this object, which gives the link its own fork and the
 * calls built on it, failing with ENOSYS, and leaves asyncify out.
 *
 * Hidden: each module of such a program binds its own calls here, and none
 * exports a fork that another module's calls would bind to. posix_spawn,
 * system and popen do not fork (a vfork-style clone, a new process gex
 * starts at the image's entry) and keep working. */
#include <errno.h>
#include <sys/types.h>

#define NOFORK __attribute__((visibility("hidden")))

NOFORK pid_t fork(void)
{
	errno = ENOSYS;
	return -1;
}

NOFORK pid_t vfork(void)
{
	errno = ENOSYS;
	return -1;
}

NOFORK pid_t _Fork(void)
{
	errno = ENOSYS;
	return -1;
}

NOFORK int daemon(int nochdir, int noclose)
{
	(void)nochdir;
	(void)noclose;
	errno = ENOSYS;
	return -1;
}

struct termios;
struct winsize;

NOFORK pid_t forkpty(int *master, char *name, const struct termios *tio, const struct winsize *ws)
{
	(void)master;
	(void)name;
	(void)tio;
	(void)ws;
	errno = ENOSYS;
	return -1;
}
