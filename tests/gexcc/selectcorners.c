/* select corners (gnulib test-select): a pipe's write end is ready for
 * writing only, not reading; a closed descriptor in any set is EBADF;
 * negative nfds is EINVAL. */
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/select.h>
#include <unistd.h>

int main(void)
{
	int p[2];
	fd_set r, w, x;
	struct timeval tv = {0, 0};
	pipe(p);
	FD_ZERO(&r); FD_ZERO(&w); FD_ZERO(&x);
	FD_SET(p[1], &r); FD_SET(p[1], &w); FD_SET(p[1], &x);
	int n = select(p[1] + 1, &r, &w, &x, &tv);
	printf("write end: %d ready, read %d write %d\n", n, FD_ISSET(p[1], &r) != 0, FD_ISSET(p[1], &w) != 0);
	int bad = dup(0);
	close(bad);
	const char *names[] = {"rfds", "wfds", "xfds"};
	for (int k = 0; k < 3; k++) {
		FD_ZERO(&r); FD_ZERO(&w); FD_ZERO(&x);
		FD_SET(bad, k == 0 ? &r : k == 1 ? &w : &x);
		tv.tv_sec = 0;
		errno = 0;
		n = select(bad + 1, &r, &w, &x, &tv);
		printf("closed fd in %s: %d %s\n", names[k], n, n < 0 ? strerror(errno) : "");
	}
	errno = 0;
	n = select(-1, 0, 0, 0, &tv);
	printf("nfds -1: %d %s\n", n, n < 0 ? strerror(errno) : "");
	return 0;
}
