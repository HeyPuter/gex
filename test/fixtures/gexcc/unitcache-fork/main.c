/* unitcache-fork: the page's unit cache and the dense base, across a fork.
 *
 * A process numbers a library's blocks as it loads it, so the two processes
 * load the libraries in opposite orders: A libfoo, then libbar; B libbar,
 * then libfoo. The unit cache hands B the Modules A compiled for libfoo,
 * whose dense section names A's base, and B numbers them at its own, above
 * libbar's. B forks; the child adopts B's modules (A's among them) and calls
 * both. A launcher that started A and B as copies of itself, or as fresh
 * processes, gives both the same numbering whatever order they call in,
 * which is why the order is in the loads. See test/gexcc/unitcache-fork.
 *   t          launch A, then B
 *   t a | t b  the two processes */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
#include <dlfcn.h>
#include <stdlib.h>
/* A Worker's console lines reach run-node by message; a process that exits at
 * once can lose its last ones (the GEX_DENSE_DEBUG lines the test greps for). */
#define LINGER 500000
static int (*foo_f)(int), (*foo_g)(int), (*bar_a_all)(int), (*bar_b_all)(int);

static void *lib(const char *name)
{
	void *h = dlopen(name, RTLD_NOW);
	if (!h) { printf("dlopen %s: %s\n", name, dlerror()); exit(2); }
	return h;
}

static void load(int foo_first)
{
	void *f = foo_first ? lib("libfoo.so") : 0;
	void *b = lib("libbar.so");
	if (!f) f = lib("libfoo.so");
	foo_f = (int (*)(int))dlsym(f, "foo_f");
	foo_g = (int (*)(int))dlsym(f, "foo_g");
	bar_a_all = (int (*)(int))dlsym(b, "bar_a_all");
	bar_b_all = (int (*)(int))dlsym(b, "bar_b_all");
}

static int run(const char *arg)
{
	pid_t p = fork();
	if (p == 0) {
		execl("/bin/t", "/bin/t", arg, (char *)0);
		_exit(127);
	}
	int st = 0;
	waitpid(p, &st, 0);
	return WIFEXITED(st) ? WEXITSTATUS(st) : 128;
}

int main(int argc, char **argv)
{
	if (argc < 2) {
		int a = run("a");
		int b = run("b");
		printf("launcher: a=%d b=%d\n", a, b);
		return a || b;
	}
	load(!strcmp(argv[1], "a"));
	if (!strcmp(argv[1], "a")) {
		printf("a: bar=%d\n", bar_a_all(50));
		printf("a: f=%d g=%d\n", foo_f(1000), foo_g(1000));
		fflush(stdout);
		usleep(LINGER);
		return 0;
	}
	printf("b: f=%d g=%d\n", foo_f(1000), foo_g(1000));
	printf("b: bar=%d\n", bar_b_all(50));
	fflush(stdout);
	pid_t c = fork();
	if (c == 0) {
		printf("child: f=%d g=%d\n", foo_f(2000), foo_g(2000));
		printf("child: bar=%d\n", bar_b_all(60));
		fflush(stdout);
		usleep(LINGER);
		_exit(0);
	}
	int st = 0;
	waitpid(c, &st, 0);
	printf("b: child status %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
	return WIFEXITED(st) && WEXITSTATUS(st) == 0 ? 0 : 1;
}
