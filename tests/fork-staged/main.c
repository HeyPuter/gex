/* Detached starts from two threads at once, as Qt's startDetached makes
   them: a vfork child (in its parent's memory) forks the grandchild and
   exits, and the parent goes on to a read that waits, while the other thread
   forks too. Each grandchild checks what it holds across its fork. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
static inline long sys3(long nr, long a, long b, long c)
{
	register long x8 __asm__("x8") = nr;
	register long x0 __asm__("x0") = a, x1 __asm__("x1") = b, x2 __asm__("x2") = c;
	__asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
	return x0;
}
static inline long sys_clone(long flags)
{
	register long x8 __asm__("x8") = 220;
	register long x0 __asm__("x0") = flags;
	register long x1 __asm__("x1") = 0, x2 __asm__("x2") = 0, x3 __asm__("x3") = 0, x4 __asm__("x4") = 0;
	__asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3), "r"(x4) : "memory");
	return x0;
}
__attribute__((noinline)) static long mix(long a, long b) { return a * 31 + (b ^ (a >> 3)); }
static int out[2];
__attribute__((noinline)) static void grandchild(long a, long b, long c)
{
	char r = c == mix(a, b) ? 'Y' : 'N';
	sys3(64, out[1], (long)&r, 1);
	sys3(94, 0, 0, 0);
}
__attribute__((noinline)) static void detached(int rep, int wake[2])
{
	long a = 0x5150 + rep, b = 0x7a7a, c = mix(a, b);
	long p = sys_clone(0x100 | 0x4000 | 17); /* CLONE_VM | CLONE_VFORK | SIGCHLD */
	if (p == 0) {
		long q = sys_clone(17);
		if (q == 0) grandchild(a, b, c);
		sys3(94, q < 0 ? 3 : 0, 0, 0);
	}
	int st;
	waitpid(p, &st, 0);
	/* Back to its loop: a read that waits. */
	char buf[512];
	read(wake[0], buf, sizeof buf);
}
static int n;
static void *thr(void *arg)
{
	int wake[2];
	if (pipe(wake)) return 0;
	for (int i = 0; i < n; i++) {
		pid_t w = fork();
		if (w == 0) { usleep(20000); write(wake[1], "w", 1); _exit(0); }
		detached(i, wake);
		waitpid(w, 0, 0);
	}
	return arg;
}
int main(int argc, char **argv)
{
	n = argc > 1 ? atoi(argv[1]) : 10;
	int threads = argc > 2 ? atoi(argv[2]) : 1;
	setvbuf(stdout, NULL, _IONBF, 0);
	if (pipe(out)) return 1;
	if (threads) {
		pthread_t t[2];
		for (int k = 0; k < 2; k++) pthread_create(&t[k], NULL, thr, NULL);
		for (int k = 0; k < 2; k++) pthread_join(t[k], NULL);
	} else thr(NULL);
	close(out[1]);
	int y = 0, no = 0;
	char c;
	usleep(300000);
	while (read(out[0], &c, 1) == 1) { if (c == 'Y') y++; else no++; }
	printf("Y=%d N=%d of %d\n", y, no, n * (threads ? 2 : 1));
	_exit(0);
}
