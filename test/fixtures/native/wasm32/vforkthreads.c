/* vfork, many times, with a sibling thread parked in the parent: the child
 * is a copy of memory as the forking thread left it at the call, never as
 * it is later. In the browser runtime any Worker of the process could claim
 * a staged fork, so the parked thread's Worker sometimes copied memory after
 * the parent had run on and unmapped the record the child starts on; the
 * child trapped reading its thread pointer (exit 137, about 1 in 125). */
#include <pthread.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

static void *park(void *a)
{
	for (;;) pause();
	return a;
}

int main(void)
{
	enum { N = 300 };
	int bad = 0;
	pthread_t t;
	pthread_create(&t, 0, park, 0);
	for (int i = 0; i < N; i++) {
		pid_t pid = vfork();
		if (pid == 0) _exit(9);
		int st;
		if (waitpid(pid, &st, 0) != pid) { printf("waitpid failed at %d\n", i); return 1; }
		int code = WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
		if (code != 9 && bad++ < 5) printf("vfork %d: child exit %d\n", i, code);
	}
	printf("%d vforks, %d bad\n", N, bad);
	return bad != 0;
}
