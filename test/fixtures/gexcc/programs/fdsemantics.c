/* Descriptor and open semantics gnulib's tests check (grep, sed, coreutils
 * all carry them): RLIMIT_NOFILE bounds dup2/F_DUPFD, bad descriptors are
 * EBADF, O_CREAT works read-only, a directory does not open for writing,
 * and *at() relative to a non-directory is ENOTDIR. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <unistd.h>

#define T(e) printf("%-44s %s\n", #e, (e) ? "ok" : strerror(errno))

int main(void)
{
	struct rlimit rl;
	int v = 1;
	T(getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur > 2 && rl.rlim_cur <= rl.rlim_max);
	int lim = (int)rl.rlim_cur;
	T(getdtablesize() == lim);
	T(dup2(0, lim) == -1 && errno == EBADF);
	T(fcntl(0, F_DUPFD, lim) == -1 && errno == EINVAL);
	T(fcntl(0, F_DUPFD_CLOEXEC, -1) == -1 && errno == EINVAL);
	T(fcntl(-1, F_GETFD) == -1 && errno == EBADF);
	T(setsockopt(-1, SOL_SOCKET, SO_REUSEADDR, &v, sizeof v) == -1 && errno == EBADF);
	rl.rlim_cur = 64;
	T(setrlimit(RLIMIT_NOFILE, &rl) == 0);
	T(dup2(0, 64) == -1 && errno == EBADF);
	T(dup2(0, 63) == 63);
	rl.rlim_cur = rl.rlim_max + 1;
	T(setrlimit(RLIMIT_NOFILE, &rl) == -1 && errno == EINVAL);
	T(getrlimit(RLIMIT_STACK, &rl) == 0 && rl.rlim_cur == 8 << 20);
	unlink("fdsem.tmp");
	int fd = open("fdsem.tmp", O_CREAT | O_RDONLY, 0600);
	T(fd >= 0);
	T(open("fdsem.tmp", O_CREAT | O_EXCL | O_RDONLY, 0600) == -1 && errno == EEXIST);
	T(open(".", O_WRONLY) == -1 && errno == EISDIR);
	T(openat(fd, ".", O_RDONLY) == -1 && errno == ENOTDIR);
	T(fopen(".", "w") == NULL);
	unlink("fdsem.tmp");
	return 0;
}
