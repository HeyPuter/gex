/* libcb_run (a unit) calls cb (tier-1), and cb blocks: on i == 2 it reads a
   pipe that a forked child fills 100 ms later, and every call sleeps 2 ms.
   In the single-thread build each block parks this guest thread inside a
   nested call-out, which must suspend (docs/single-thread.md §7) rather
   than trap with "trying to suspend JS frames". */
#include <stdio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int libcb_run(int (*f)(int), int n);
static int fds[2];

static int cb(int i)
{
	if (i == 2) {
		char c = 0;
		if (read(fds[0], &c, 1) != 1)
			return -1000;
		printf("callback read %c\n", c);
		fflush(stdout);
	}
	nanosleep(&(struct timespec){0, 2000000}, 0);
	return i * 10;
}

int main(void)
{
	if (pipe(fds))
		return 2;
	pid_t pid = fork();
	if (pid == 0) {
		nanosleep(&(struct timespec){0, 100000000}, 0);
		write(fds[1], "w", 1);
		_exit(0);
	}
	int s = libcb_run(cb, 5);
	waitpid(pid, 0, 0);
	printf("libcb_run = %d %s\n", s, s == 100 ? "ok" : "wrong");
	return s == 100 ? 0 : 1;
}
