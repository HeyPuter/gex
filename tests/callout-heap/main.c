/* libcb_run (a unit) calls cb (tier-1) N times, and cb raises a signal: every
   call is a call-out, and every signal a trap inside it, served by the
   call-out's own loop without the Worker reaching its event loop. */
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>

int libcb_run(int (*f)(int), int n);
static volatile int got;

static void on(int s)
{
	(void)s;
	got++;
}

static int cb(int i)
{
	raise(SIGUSR1);
	return i & 1;
}

int main(int argc, char **argv)
{
	int n = argc > 1 ? atoi(argv[1]) : 1000;
	signal(SIGUSR1, on);
	int s = libcb_run(cb, n);
	printf("libcb_run %d = %d, %d signals %s\n", n, s, got, s == n / 2 && got == n ? "ok" : "wrong");
	return s == n / 2 && got == n ? 0 : 1;
}
