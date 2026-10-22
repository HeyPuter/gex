#define _GNU_SOURCE
/* Three processes run liba.so at one base: this one, a forked child that
 * inherited the mapping, and another program (other.c) that maps it while
 * this one still has it. */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
__attribute__((noinline)) static long low3(long i) { return i & 7; }
int main(int argc, char **argv) {
	const char *dir = argc > 1 ? argv[1] : ".";
	char path[4096], other[4096], fd[16];
	int p[2];
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (pipe(p)) return 1;
	snprintf(other, sizeof other, "%s/other", dir);
	snprintf(fd, sizeof fd, "%d", p[0]);
	pid_t o = fork();
	if (o == 0) {
		close(p[1]);
		execl(other, other, dir, fd, (char *)0);
		_exit(127);
	}
	close(p[0]);
	snprintf(path, sizeof path, "%s/liba.so", dir);
	void *h = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!h) { printf("dlopen %s: %s\n", path, dlerror()); return 1; }
	int (*f)(int) = (int (*)(int))dlsym(h, "f");
	printf("parent f(5)=%d\n", f(5));
	int (*forked)(void) = (int (*)(void))dlsym(h, "forked");
	printf("forked %d\n", forked());
	long (*each)(long (*)(long), long) = (long (*)(long (*)(long), long))dlsym(h, "each");
	printf("each %ld\n", each(low3, 1000000));
	pid_t c = fork();
	if (c == 0) {
		printf("child f(9)=%d\n", f(9));
		_exit(0);
	}
	int st;
	waitpid(c, &st, 0);
	/* The other program maps liba.so now, while this process holds it. */
	if (write(p[1], "g", 1) != 1) return 1;
	waitpid(o, &st, 0);
	printf("other exited %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
	return 0;
}
