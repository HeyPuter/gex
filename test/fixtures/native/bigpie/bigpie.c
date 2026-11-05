/* A position-independent executable bigger than the space gex gives one
 * below the interpreter (chromium's is 300 MB): 96 MiB of .bss here. Its
 * interpreter maps it, so it runs with its own argv[0], /proc/self/exe is
 * the program, and a re-exec of /proc/self/exe (chromium's zygote) is the
 * program again. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

static char big[96 << 20];

int main(int argc, char **argv)
{
	char exe[256] = {0};
	big[0] = 1;
	big[sizeof big - 1] = 2;
	readlink("/proc/self/exe", exe, sizeof exe - 1);
	printf("%s: argv[0] %s, %d args, exe %s, ends %d %d\n", argc > 1 ? argv[1] : "parent", argv[0], argc, exe,
	       big[0], big[sizeof big - 1]);
	fflush(stdout);
	if (argc > 1)
		return 0;
	pid_t p = fork();
	if (p == 0) {
		execl("/proc/self/exe", "renamed", "child", (char *)0);
		_exit(127);
	}
	int st;
	waitpid(p, &st, 0);
	printf("child exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
	return 0;
}
