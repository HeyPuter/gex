/* fork without exec: parent and child both go on, each with its own memory;
 * fork from deep in a call chain reached through a function pointer (locals
 * of every frame survive in both);
 * fork in a threaded program (only the calling thread is in the child);
 * vfork whose child only _exits (vfork is fork on gex: libgex). */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int shared_counter = 100;

static int status_of(pid_t pid)
{
	int st;
	if (waitpid(pid, &st, 0) != pid) return -1;
	return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

/* A call chain whose frames all hold live state across the fork. */
static int deep(int depth, int acc, char *buf)
{
	char local[32];
	snprintf(local, sizeof local, "d%d", depth);
	if (depth == 0) {
		pid_t pid = fork();
		if (pid < 0) return -1;
		if (pid == 0) {
			strcat(buf, "c");
			return acc + 1000;
		}
		int st = status_of(pid);
		strcat(buf, "p");
		return acc + st;
	}
	int r = deep(depth - 1, acc + depth, buf);
	/* the frame's own local, intact on the way back */
	if (strcmp(local + 1, "") == 0 || atoi(local + 1) != depth) return -9999;
	return r;
}

static void *spin(void *arg)
{
	for (;;) pause();
	return arg;
}

int main(void)
{
	setvbuf(stdout, 0, _IONBF, 0);
	/* 1: both go on, separate memory */
	pid_t pid = fork();
	if (pid < 0) { perror("fork"); return 1; }
	if (pid == 0) {
		shared_counter += 1;
		printf("child counter %d\n", shared_counter);
		_exit(7);
	}
	int st = status_of(pid);
	printf("parent counter %d, child exit %d\n", shared_counter, st);

	/* 2: deep, entered through a function pointer */
	char buf[64] = "";
	int (*volatile chain)(int, int, char *) = deep;
	int r = chain(20, 0, buf);
	if (r >= 1000) {
		/* the child, back at the top of the chain */
		printf("deep child %d %s\n", r, buf);
		exit(r - 1000 == 210 ? 3 : 4);
	}
	printf("deep parent %d %s\n", r, buf);

	/* 3: threads */
	pthread_t t;
	pthread_create(&t, 0, spin, 0);
	pid = fork();
	if (pid == 0) {
		/* one thread here: this one */
		printf("threaded child\n");
		_exit(5);
	}
	printf("threaded parent, child exit %d\n", status_of(pid));

	/* 4: vfork */
	pid = vfork();
	if (pid == 0) _exit(9);
	printf("vfork child exit %d\n", status_of(pid));
	return 0;
}
