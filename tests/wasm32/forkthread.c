/* fork and a Wine-style clone (CLONE_VFORK on a stack of its own) from a
 * thread other than the first: the child runs on the forking thread's
 * registers. The browser runtime entered every forked child on the first
 * thread's, so a child of a second thread started on the first thread's
 * stack and trapped there (W3: Wine's spawn from services.exe's RPC thread,
 * "table index is out of bounds" for rpcss and every service child). */
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static int code(pid_t pid)
{
	int st;
	if (pid < 0 || waitpid(pid, &st, 0) != pid) return -1;
	return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

static int child_fn(void *arg)
{
	_exit(*(int *)arg);
}

/* As Wine's spawn_process does on wasm32 (dlls/ntdll/unix/process.c). */
static pid_t fork_call(int *arg)
{
	const size_t size = 256 * 1024;
	char *stack = mmap(0, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (stack == MAP_FAILED) return -1;
	pid_t pid = clone(child_fn, stack + size, CLONE_VFORK | SIGCHLD, arg);
	munmap(stack, size);
	return pid;
}

static void *second(void *p)
{
	int *r = p;
	pid_t pid = fork();
	if (pid == 0) _exit(7);
	r[0] = code(pid);
	int eleven = 11;
	r[1] = code(fork_call(&eleven));
	return 0;
}

int main(void)
{
	int r[2] = { -2, -2 };
	pthread_t t;
	pthread_create(&t, 0, second, r);
	pthread_join(t, 0);
	int five = 5;
	int m = code(fork_call(&five));
	printf("fork from a second thread: child exit %d\n", r[0]);
	printf("clone from a second thread: child exit %d\n", r[1]);
	printf("clone from the first thread: child exit %d\n", m);
	return !(r[0] == 7 && r[1] == 11 && m == 5);
}
