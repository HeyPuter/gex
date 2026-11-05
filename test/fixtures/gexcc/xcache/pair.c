/* Two processes whose store saves overlap: the child (/bin/t) exits and
 * starts its save, and this process (pid 1) exits while that save is still
 * being written (test/gexcc/xcache slows each write with GEX_XCACHE_DELAY_MS). With an
 * argument, pid 1 first runs code no earlier copy of the store has, so it
 * has a save of its own to make. */
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

__attribute__((noinline)) static int late(int x)
{
	int s = 0;
	for (int i = 0; i < x; i++) s += (i * 3) ^ (s >> 2);
	return s;
}

int main(int argc, char **argv)
{
	pid_t p = fork();
	if (p == 0) {
		execl("/bin/t", "/bin/t", (char *)0);
		_exit(127);
	}
	int st = 0;
	waitpid(p, &st, 0);
	/* The child's exit is published before its save: let it take its copy. */
	sleep(1);
	if (argc > 1) printf("late %d\n", late(1000));
	return WIFEXITED(st) ? WEXITSTATUS(st) : 1;
}
