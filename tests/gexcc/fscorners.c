/* More file corners: O_NOFOLLOW on a symlink is ELOOP; chown of a missing
 * file fails (ownership is not kept, but the file must exist); nanosleep
 * rejects bad times; getgroups' size check; sigaltstack. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define T(e) printf("%-56s %s\n", #e, (e) ? "ok" : strerror(errno))

int main(void)
{
	static char stk[65536];
	gid_t g[256];
	unlink("fc.l");
	T(symlink("fscorners.tmp", "fc.l") == 0);
	close(open("fscorners.tmp", O_CREAT | O_WRONLY, 0644));
	T(open("fc.l", O_RDONLY | O_NOFOLLOW) == -1 && errno == ELOOP);
	T(open("fc.l", O_RDONLY) >= 0);
	T(chown("fc.missing", getuid(), getgid()) == -1 && errno == ENOENT);
	T(lchown("fc.l", getuid(), getgid()) == 0);
	T(fchownat(AT_FDCWD, "", getuid(), getgid(), 0) == -1 && errno == ENOENT);
	struct timespec ts = {-1, 0};
	T(nanosleep(&ts, 0) == -1 && errno == EINVAL);
	ts.tv_sec = 0;
	ts.tv_nsec = 1000000000;
	T(nanosleep(&ts, 0) == -1 && errno == EINVAL);
	int n = getgroups(0, 0);
	T(n >= 0 && getgroups(256, g) == n);
	stack_t ss = {0};
	ss.ss_sp = stk;
	ss.ss_size = sizeof stk;
	T(sigaltstack(&ss, 0) == 0);
	unlink("fc.l");
	unlink("fscorners.tmp");
	return 0;
}
