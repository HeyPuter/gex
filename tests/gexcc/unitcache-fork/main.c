/* unitcache-fork: the page's unit cache and the dense base, across a fork.
 *
 * Process A runs bar_a_* (numbering many blocks of its own), then libfoo, so
 * libfoo's Modules carry a high dense base. Process B runs libfoo first: the
 * unit cache hands it A's Modules, which B numbers low. B then runs bar_b_*,
 * numbering its own blocks up through where A put libfoo. B forks; the child
 * adopts B's modules (A's among them) and calls both. See run.sh.
 *   t          launch A, then B
 *   t a | t b  the two processes */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
/* A Worker's console lines reach run-node by message; a process that exits at
 * once can lose its last ones (the GEX_DENSE_DEBUG lines the test greps for). */
#define LINGER 500000
int foo_f(int);
int foo_g(int);
int bar_a_all(int);
int bar_b_all(int);

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
