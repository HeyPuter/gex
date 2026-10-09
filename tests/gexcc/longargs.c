/* execve with long arguments: a 20 KiB argument and 2000 arguments reach
 * the new program intact (a make recipe of several KiB passed to sh -c,
 * automake's test-suite.log rule); one past MAX_ARG_STRLEN is E2BIG. */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	if (argc > 1 && strcmp(argv[1], "child") == 0) {
		size_t total = 0;
		for (int i = 2; i < argc; i++)
			total += strlen(argv[i]);
		printf("child: %d args, %zu bytes\n", argc, total);
		return 0;
	}
	static char big[20000], huge[140000];
	memset(big, 'x', sizeof big - 1);
	memset(huge, 'y', sizeof huge - 1);
	char *v1[] = {argv[0], "child", big, 0};
	char **v2 = calloc(2003, sizeof *v2);
	v2[0] = argv[0];
	v2[1] = "child";
	for (int i = 2; i < 2002; i++)
		v2[i] = "abcdefgh";
	char *v3[] = {argv[0], "child", huge, 0};
	char **vs[] = {v1, v2};
	for (int k = 0; k < 2; k++) {
		fflush(stdout);
		pid_t p = fork();
		if (p == 0) {
			execv("/proc/self/exe", vs[k]);
			_exit(127);
		}
		waitpid(p, 0, 0);
	}
	execv("/proc/self/exe", v3);
	printf("huge: %s\n", errno == E2BIG ? "E2BIG" : strerror(errno));
	return 0;
}
