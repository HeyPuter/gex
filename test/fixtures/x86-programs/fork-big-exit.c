/* A fork child that grows its memory past 200 MiB and exits cleanly 2 s later, so its
 * Worker is given back to the pool (web/test/check-given-back-memory.mjs);
 * the parent waits for it and then sleeps for the seconds given (default 6)
 * while the check looks at what the given-back Worker still holds. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv) {
	setvbuf(stdout, NULL, _IONBF, 0);
	pid_t p = fork();
	if (p < 0) return 2;
	if (p == 0) {
		size_t n = 220u << 20;
		char *b = malloc(n);
		if (!b) _exit(3);
		memset(b, 1, n);
		sleep(2); /* long enough to be seen holding it */
		_exit(0);
	}
	int st = 0;
	waitpid(p, &st, 0);
	printf("child exited %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
	sleep(argc > 1 ? atoi(argv[1]) : 6);
	printf("parent done\n");
	return 0;
}
