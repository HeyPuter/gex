/* A session snapshot's test program: three threads, each stopped in a
 * different wait with values on its stack that only survive if its frames
 * do. Run whole, it prints the same lines as run, snapshotted at "parked",
 * then restored (test/snapshot/snapshot).
 *   - "cond": pthread_cond_wait (a futex wait) until main says go;
 *   - "epoll": epoll_wait on a pipe main writes to;
 *   - main: a 4 s nanosleep (test/snapshot/snapshot snapshots it 2 s in: restored,
 *     it sleeps what was left, on a monotonic clock that did not count
 *     the time saved), then writes the pipe and signals the condition. */
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <time.h>
#include <unistd.h>

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int go, ready;
static int fds[2];

static unsigned mix(unsigned h, unsigned v) { return (h ^ v) * 16777619u; }

static void up(void)
{
	pthread_mutex_lock(&mu);
	ready++;
	pthread_cond_broadcast(&cv);
	pthread_mutex_unlock(&mu);
}

static void *cond_thread(void *arg)
{
	volatile unsigned local[16];
	unsigned h = 2166136261u ^ (unsigned)(unsigned long)arg;
	for (int i = 0; i < 16; i++) local[i] = h = mix(h, i * 7);
	pthread_mutex_lock(&mu);
	ready++;
	pthread_cond_broadcast(&cv);
	while (!go) pthread_cond_wait(&cv, &mu);
	pthread_mutex_unlock(&mu);
	unsigned s = 0;
	for (int i = 0; i < 16; i++) s = mix(s, local[i]);
	printf("cond: woke, stack %08x\n", s);
	return (void *)(unsigned long)s;
}

static void *epoll_thread(void *arg)
{
	volatile unsigned local[16];
	unsigned h = 2166136261u ^ (unsigned)(unsigned long)arg;
	for (int i = 0; i < 16; i++) local[i] = h = mix(h, i * 13);
	int ep = epoll_create1(0);
	struct epoll_event ev = { .events = EPOLLIN, .data.u32 = 77 };
	epoll_ctl(ep, EPOLL_CTL_ADD, fds[0], &ev);
	up();
	struct epoll_event got;
	int n = epoll_wait(ep, &got, 1, -1);
	char buf[16] = {0};
	ssize_t r = read(fds[0], buf, sizeof buf - 1);
	unsigned s = 0;
	for (int i = 0; i < 16; i++) s = mix(s, local[i]);
	printf("epoll: %d ready (data %u), read %zd \"%s\", stack %08x\n", n, got.data.u32, r, buf, s);
	return (void *)(unsigned long)s;
}

int main(void)
{
	volatile unsigned local[16];
	unsigned h = 2166136261u;
	for (int i = 0; i < 16; i++) local[i] = h = mix(h, i * 31);
	if (pipe(fds)) return 1;
	pthread_t a, b;
	pthread_create(&a, 0, cond_thread, (void *)1);
	pthread_create(&b, 0, epoll_thread, (void *)2);
	pthread_mutex_lock(&mu);
	while (ready < 2) pthread_cond_wait(&cv, &mu);
	pthread_mutex_unlock(&mu);
	printf("parked\n");
	fflush(stdout);
	struct timespec t0, t1, ts = { 4, 0 };
	clock_gettime(CLOCK_MONOTONIC, &t0);
	nanosleep(&ts, 0);
	clock_gettime(CLOCK_MONOTONIC, &t1);
	double slept = (t1.tv_sec - t0.tv_sec) + (t1.tv_nsec - t0.tv_nsec) / 1e9;
	if (slept >= 4.0 && slept < 5.5) printf("main: slept 4 s\n");
	else printf("main: slept %.2f s\n", slept);
	write(fds[1], "hello", 5);
	pthread_mutex_lock(&mu);
	go = 1;
	pthread_cond_broadcast(&cv);
	pthread_mutex_unlock(&mu);
	void *ra, *rb;
	pthread_join(a, &ra);
	pthread_join(b, &rb);
	unsigned s = 0;
	for (int i = 0; i < 16; i++) s = mix(s, local[i]);
	printf("main: joined %08lx %08lx, stack %08x\n", (unsigned long)ra, (unsigned long)rb, s);
	return 0;
}
