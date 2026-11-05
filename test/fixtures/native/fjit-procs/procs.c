/* Two images at the same addresses under one function JIT (test/native/fjit-procs).
 *
 * Built twice from this file: the parent image (no CHILD) and the child image
 * (-DCHILD). The first function each defines, after the same C runtime and
 * page-aligned, sits at the same address in both: the parent's `probe`, which
 * returns a value, and the child's `__memset_gexprobe`, a name the function
 * JIT replaces with a host memset; the child's `probe` never returns. The
 * parent forks, the child execs the child image and runs. Only then does the
 * parent translate `after`, which calls its own `probe` and prints the result.
 * A translator that applied what it learned of the child image (builtins by
 * address, functions that never return) to the parent gets it wrong. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

#ifdef CHILD
/* A name the function JIT serves with a host builtin (gex_xlate::intrinsic:
 * glibc's private `__memset_*` shape), at the address of the parent's
 * `probe`. */
__attribute__((noinline, aligned(4096))) void *__memset_gexprobe(void *d, int c, unsigned long n)
{
	volatile unsigned char *p = d;
	while (n--)
		*p++ = (unsigned char)c;
	return d;
}

__attribute__((noinline, noreturn)) void probe(int x)
{
	char buf[16];
	__memset_gexprobe(buf, 'a', sizeof buf);
	if (x >= 0 && buf[15] == 'a')
		_exit(x & 7);
	for (;;)
		;
}
#else
__attribute__((noinline, aligned(4096))) int probe(int x)
{
	volatile int y = x * 3 + 1;
	return y;
}
#endif

__attribute__((noinline)) int after(int x)
{
#ifdef CHILD
	probe(x);
#else
	int r = probe(x);
	printf("after probe: %d\n", r + 1);
	return r;
#endif
}

int main(int argc, char **argv)
{
	/* Never true: keeps the read-only data the same in both images. */
	if (argc == 12345)
		printf("after probe: %d\n", argc);
	if (argc > 1 && !strcmp(argv[1], "child")) {
		printf("child running\n");
		fflush(stdout);
		int (*volatile later)(int) = after;
		later(argc + 1);
		return 1;
	}
	if (argc < 2) {
		fprintf(stderr, "usage: procs CHILD-IMAGE\n");
		return 2;
	}
	fflush(stdout);
	pid_t p = fork();
	if (p == 0) {
		execl(argv[1], argv[1], "child", (char *)0);
		_exit(99);
	}
	int st = 0;
	waitpid(p, &st, 0);
	printf("child exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
	/* Through a pointer: a direct call would put `after` and `probe` in the
	   batch translated with `main`, before the child ran. */
	int (*volatile later)(int) = after;
	int r = later(5);
	printf("parent done %d\n", r);
	return 0;
}
