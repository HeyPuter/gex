/* tests/wasm64: the LP64 layout W64's Unix side shares with gex-kernel's
 * x86-64 ABI (docs/wine-native-x64.md §1.1). Printed natively (x86-64 glibc)
 * and by the lowered wasm64 program (musl wasm64); the run must match. Then
 * a few calls whose structures the kernel fills: stat, clock_gettime,
 * getrlimit, uname, a pipe through poll. */
#define _GNU_SOURCE
#include <dirent.h>
#include <poll.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

#define S(t) printf("%-22s %zu\n", #t, sizeof(t))
#define O(t, f) printf("%-22s %zu\n", #t "." #f, offsetof(t, f))

int main(void)
{
	S(long); S(void *); S(size_t); S(off_t); S(time_t); S(wchar_t);
	S(struct stat); O(struct stat, st_mode); O(struct stat, st_size); O(struct stat, st_mtim);
	S(struct timespec); S(struct timeval); S(struct iovec); S(struct msghdr);
	O(struct msghdr, msg_iov); O(struct msghdr, msg_control); O(struct msghdr, msg_flags);
	S(struct cmsghdr); S(struct pollfd); S(struct rlimit); S(siginfo_t); S(stack_t);
	O(stack_t, ss_size); S(struct sockaddr_storage); O(struct dirent, d_name);
	printf("char is %s\n", (char)-1 < 0 ? "signed" : "unsigned");

	struct stat st;
	if (stat("/", &st) || !S_ISDIR(st.st_mode)) return 1;
	puts("stat / is a directory");
	struct timespec a, b;
	clock_gettime(CLOCK_MONOTONIC, &a);
	clock_gettime(CLOCK_MONOTONIC, &b);
	printf("monotonic %s\n", (b.tv_sec > a.tv_sec || (b.tv_sec == a.tv_sec && b.tv_nsec >= a.tv_nsec)) ? "ok" : "backwards");
	struct rlimit rl;
	printf("getrlimit %d\n", getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur > 0);
	struct utsname u;
	printf("uname %d\n", uname(&u) == 0 && strlen(u.sysname) > 0);
	int p[2];
	if (pipe(p)) return 2;
	write(p[1], "lp64", 4);
	struct pollfd pf = { p[0], POLLIN, 0 };
	char buf[8] = { 0 };
	int n = poll(&pf, 1, 1000);
	printf("poll %d revents %d\n", n, pf.revents & POLLIN);
	printf("read %zd %s\n", read(p[0], buf, sizeof buf - 1), buf);
	/* stat's owner is the process's own ids (x86-64's stat and fstat, not
	 * statx): Wine refuses a prefix it does not own */
	struct stat ps;
	printf("pipe owned by getuid %d\n", fstat(p[0], &ps) == 0 && ps.st_uid == getuid() && ps.st_gid == getgid());
	long big = 1L << 40;
	printf("long arithmetic %ld\n", big / 3);
	return 0;
}
