/* Traps of every common kind, each checked by its result: signals raised in
   call-outs (libcb_run, a unit, calls cb on tier-1 N times) and outside, a condition
   variable passed between two threads N times (futex waits), sleeps, and a
   fork whose child exits with a status the parent checks. */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int libcb_run(int (*f)(int), int n);
static volatile int got;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int turn, rounds;

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

/* Waits for its turn (1), hands it back (0). */
static void *pong(void *arg)
{
	(void)arg;
	for (int i = 0; i < rounds; i++) {
		pthread_mutex_lock(&mu);
		while (turn != 1)
			pthread_cond_wait(&cv, &mu);
		turn = 0;
		pthread_cond_signal(&cv);
		pthread_mutex_unlock(&mu);
	}
	return 0;
}

int main(int argc, char **argv)
{
	int n = argc > 1 ? atoi(argv[1]) : 1000, bad = 0;
	signal(SIGUSR1, on);
	int s = libcb_run(cb, n);
	int ok = s == n / 2 && got == n;
	bad |= !ok;
	printf("call-outs: libcb_run %d = %d, %d signals %s\n", n, s, got, ok ? "ok" : "wrong");

	got = 0;
	for (int i = 0; i < n; i++)
		raise(SIGUSR1);
	ok = got == n;
	bad |= !ok;
	printf("signals: %d raised, %d handled %s\n", n, got, ok ? "ok" : "wrong");

	rounds = n / 4;
	pthread_t t;
	pthread_create(&t, 0, pong, 0);
	for (int i = 0; i < rounds; i++) {
		pthread_mutex_lock(&mu);
		turn = 1;
		pthread_cond_signal(&cv);
		while (turn != 0)
			pthread_cond_wait(&cv, &mu);
		pthread_mutex_unlock(&mu);
	}
	pthread_join(t, 0);
	printf("threads: %d rounds ok\n", rounds);

	struct timespec a, b, d = {0, 2000000};
	clock_gettime(CLOCK_MONOTONIC, &a);
	for (int i = 0; i < 10; i++)
		nanosleep(&d, 0);
	clock_gettime(CLOCK_MONOTONIC, &b);
	long ms = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
	ok = ms >= 20;
	bad |= !ok;
	printf("sleeps: 10 of 2 ms took %ld ms %s\n", ms, ok ? "ok" : "wrong");

	pid_t p = fork();
	if (p == 0)
		_exit(42);
	int st = 0;
	ok = p > 0 && waitpid(p, &st, 0) == p && WIFEXITED(st) && WEXITSTATUS(st) == 42;
	bad |= !ok;
	printf("fork: child exited %d %s\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1, ok ? "ok" : "wrong");
	return bad;
}
