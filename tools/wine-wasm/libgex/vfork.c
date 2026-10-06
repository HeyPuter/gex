/* libgex: vfork is fork (fork unwinds and rewinds the wasm stack around the
 * kernel's copy, musl's src/process/wasm32/__gex_fork.c); a vfork child
 * that only execs or _exits cannot tell the difference. musl's generic
 * vfork issues SYS_fork through syscall(), which does not reach that
 * driver; wcc links this library before libc, so this one is used. */
#define _GNU_SOURCE
#include <unistd.h>

pid_t vfork(void)
{
	return fork();
}
