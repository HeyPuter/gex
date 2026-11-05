/* execve with other threads about, as Linux does it: every other thread of
 * the process goes, the program is replaced, and the pid stays.
 *
 * 1. A forked child that execs while a thread it has just created is still
 *    starting. The browser runtime's thread Worker came up after the exec had
 *    dropped the thread and threw "this process has no thread N", which
 *    crashed the page.
 * 2. execve from a thread other than the first (Linux's de_thread): the
 *    exec'ing thread takes over the process, the first thread and a third one
 *    blocked in a wait go with the old program. gexc refused this ("execve
 *    from a non-main thread is not supported") and the page ran the new
 *    program with the old threads' Workers still going. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static char *self;

static int code(pid_t pid)
{
	int st;
	if (pid < 0 || waitpid(pid, &st, 0) != pid) return -1;
	return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

static void *idle(void *arg)
{
	(void)arg;
	for (;;) pause();
	return NULL;
}

static void *spin(void *arg)
{
	volatile long n = 0;
	(void)arg;
	for (;;) n++;
	return NULL;
}

static void *exec_it(void *arg)
{
	char *argv[] = {self, (char *)arg, NULL};
	execv(self, argv);
	_exit(99);
}

int main(int argc, char **argv)
{
	int i, c, ok = 0, n = 12;

	self = "/proc/self/exe";
	(void)argv;
	if (argc > 1) return atoi(argv[1]);

	/* 1 */
	for (i = 0; i < n; i++) {
		pid_t pid = fork();
		if (pid == 0) {
			pthread_t t;
			char *av[] = {self, "7", NULL};
			pthread_create(&t, NULL, i & 1 ? spin : idle, NULL);
			execv(self, av);
			_exit(99);
		}
		c = code(pid);
		if (c != 7) fprintf(stderr, "child %d: %d\n", i, c);
		ok += c == 7;
	}
	printf("exec with a thread starting: %d of %d\n", ok, n);

	/* 2 */
	ok = 0;
	for (i = 0; i < 4; i++) {
		pid_t pid = fork();
		if (pid == 0) {
			pthread_t a, b;
			pthread_create(&a, NULL, idle, NULL);
			pthread_create(&b, NULL, exec_it, "9");
			pthread_join(b, NULL);
			_exit(98);
		}
		c = code(pid);
		if (c != 9) fprintf(stderr, "child %d: %d\n", i, c);
		ok += c == 9;
	}
	printf("exec from a second thread: %d of 4\n", ok);
	fflush(stdout);
	return 0;
}
