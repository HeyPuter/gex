/* Signals reach code that never makes a system call (GexPoll's loop
 * safepoints): alarm() with a handler stops a busy loop, and so does the
 * default action (SIGALRM terminates). */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t fired;
static void on_alarm(int s) { (void)s; fired = 1; }

static unsigned long spin(volatile unsigned long *x)
{
	unsigned long n = 0;
	while (!fired)
		n += (*x)++ & 1;
	return n;
}

int main(void)
{
	volatile unsigned long x = 0;
	signal(SIGALRM, on_alarm);
	alarm(1);
	spin(&x);
	printf("handler ran: %d\n", (int)fired);
	fflush(stdout);
	/* default action: the child dies of SIGALRM in its loop */
	pid_t p = fork();
	if (p == 0) {
		signal(SIGALRM, SIG_DFL);
		alarm(1);
		for (;;)
			x++;
	}
	int st;
	waitpid(p, &st, 0);
	printf("child: signaled %d sig %d\n", WIFSIGNALED(st), WIFSIGNALED(st) ? WTERMSIG(st) : 0);
	return 0;
}
