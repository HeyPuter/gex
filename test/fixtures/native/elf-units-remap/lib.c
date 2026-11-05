/* liba.so (-DK=3 -DC=1) and libb.so (-DK=7 -DC=-2): the same layout, different code. */
#include <sys/wait.h>
#include <unistd.h>
__attribute__((noinline)) static int inner(int x) { return x * K + C; }
int f(int x) { int s = 0; for (int i = 0; i < x; i++) s += inner(i); return s + inner(x); }
/* A fork made from the library's own code (procs.sh): under its unit, libc's
 * fork runs in a call-out, and the child must still be made. */
int forked(void) {
	pid_t p = fork();
	if (p == 0) _exit(K);
	int st = 0;
	waitpid(p, &st, 0);
	return WIFEXITED(st) ? WEXITSTATUS(st) : -1;
}
/* Calls back into the program n times (procs.sh): each call is a call-out
 * from the unit, and the callback's return reaches the watched slot often
 * right after its safepoint, which leaves the context PC on the `ret`. */
long each(long (*cb)(long), long n) {
	long s = 0;
	for (long i = 0; i < n; i++) s += cb(i);
	return s;
}
