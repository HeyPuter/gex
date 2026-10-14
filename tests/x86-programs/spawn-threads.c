/* posix_spawn from a multithreaded parent: libc clones with
 * CLONE_VM | CLONE_VFORK onto a stack of its own, the child applies its file
 * actions and attributes and execs, and only the calling thread waits. In the
 * page the child runs in the calling thread's Worker, in the parent's memory
 * (shims/browser/src/vfork.rs), so this checks what that must keep apart:
 *   - the child's descriptors (its dup2 onto 1 is not the parent's 1);
 *   - its signal mask and dispositions (setsigmask, setsigdef);
 *   - its exit status and pid, which the parent's wait sees;
 *   - an exec that fails (ENOENT back from posix_spawn, or status 127);
 *   - the parent's other threads, which run on throughout.
 * Run with no arguments; it spawns itself (argv[0]) with "child N". */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

#define SPAWNS 24
#define WORKERS 3

static atomic_int stop;
static atomic_long ticks[WORKERS];

static void *worker(void *p)
{
	int i = (int)(long)p;
	struct timespec ts = { 0, 200000 };
	while (!atomic_load(&stop)) {
		atomic_fetch_add(&ticks[i], 1);
		/* A call too, so the thread is in and out of the kernel while
		 * the child is in it. */
		if (getpid() <= 0) abort();
		nanosleep(&ts, 0);
	}
	return 0;
}

static int fcntl_ok(int fd) { return fcntl(fd, F_GETFD) != -1; }

static int child(int n)
{
	/* What the parent asked for: SIGUSR1 blocked, SIGUSR2 default. */
	sigset_t m;
	sigprocmask(SIG_BLOCK, 0, &m);
	struct sigaction sa;
	sigaction(SIGUSR2, 0, &sa);
	/* fd 1 is the pipe the parent gave; fd 5 was closed for us. */
	char line[64];
	int len = snprintf(line, sizeof line, "child %d usr1=%d usr2=%s fd5=%s\n", n,
		sigismember(&m, SIGUSR1), sa.sa_handler == SIG_DFL ? "dfl" : "set",
		fcntl_ok(5) ? "open" : "closed");
	if (write(1, line, len) != len) return 99;
	return n % 7;
}

static void on_usr2(int s) { (void)s; }

int main(int argc, char **argv)
{
	if (argc == 3 && !strcmp(argv[1], "child")) return child(atoi(argv[2]));
	const char *self = argv[0];

	pthread_t t[WORKERS];
	for (int i = 0; i < WORKERS; i++) pthread_create(&t[i], 0, worker, (void *)(long)i);

	/* The parent: SIGUSR2 handled, SIGUSR1 not blocked, a descriptor at 5
	 * the child is told to close. */
	signal(SIGUSR2, on_usr2);
	int five = dup2(0, 5);

	int ok = 0;
	for (int i = 0; i < SPAWNS; i++) {
		int p[2];
		if (pipe(p)) return 2;
		posix_spawn_file_actions_t fa;
		posix_spawn_file_actions_init(&fa);
		posix_spawn_file_actions_adddup2(&fa, p[1], 1);
		posix_spawn_file_actions_addclose(&fa, p[0]);
		posix_spawn_file_actions_addclose(&fa, p[1]);
		posix_spawn_file_actions_addclose(&fa, 5);
		posix_spawnattr_t at;
		posix_spawnattr_init(&at);
		sigset_t m, d;
		sigemptyset(&m);
		sigaddset(&m, SIGUSR1);
		sigemptyset(&d);
		sigaddset(&d, SIGUSR2);
		posix_spawnattr_setsigmask(&at, &m);
		posix_spawnattr_setsigdefault(&at, &d);
		posix_spawnattr_setflags(&at, POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF);
		char n[16];
		snprintf(n, sizeof n, "%d", i);
		char *args[] = { (char *)self, "child", n, 0 };
		pid_t pid;
		int r = posix_spawn(&pid, self, &fa, &at, args, environ);
		posix_spawn_file_actions_destroy(&fa);
		posix_spawnattr_destroy(&at);
		close(p[1]);
		if (r) {
			printf("spawn %d: %s\n", i, strerror(r));
			close(p[0]);
			continue;
		}
		char buf[128] = { 0 };
		int got = 0, k;
		while ((k = read(p[0], buf + got, sizeof buf - 1 - got)) > 0) got += k;
		close(p[0]);
		int st;
		if (waitpid(pid, &st, 0) != pid) {
			printf("spawn %d: wait: %s\n", i, strerror(errno));
			continue;
		}
		int code = WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
		char want[96];
		snprintf(want, sizeof want, "child %d usr1=1 usr2=dfl fd5=closed\n", i);
		if (code == i % 7 && !strcmp(buf, want)) ok++;
		else printf("spawn %d: exit %d, said \"%s\"\n", i, code, buf);
	}
	printf("%d of %d children right\n", ok, SPAWNS);

	/* The parent's own state, after all that: fd 1 is still its stdout,
	 * fd 5 still open, SIGUSR2 still handled, SIGUSR1 not blocked. */
	struct sigaction sa;
	sigaction(SIGUSR2, 0, &sa);
	sigset_t m;
	sigprocmask(SIG_BLOCK, 0, &m);
	printf("parent: fd5 %s, usr2 %s, usr1 %s\n", fcntl_ok(five) ? "open" : "closed",
		sa.sa_handler == on_usr2 ? "handled" : "lost", sigismember(&m, SIGUSR1) ? "blocked" : "open");

	/* An exec that fails: the child exits 127, or posix_spawn says ENOENT
	 * (glibc and musl both report it). */
	char *nargs[] = { "/nonexistent/program", 0 };
	pid_t pid;
	int r = posix_spawn(&pid, "/nonexistent/program", 0, 0, nargs, environ);
	if (r == 0) {
		int st;
		waitpid(pid, &st, 0);
		r = WIFEXITED(st) && WEXITSTATUS(st) == 127 ? ENOENT : -1;
	}
	printf("missing program: %s\n", r == ENOENT ? "ENOENT" : "wrong");

	atomic_store(&stop, 1);
	for (int i = 0; i < WORKERS; i++) pthread_join(t[i], 0);
	int ran = 0;
	for (int i = 0; i < WORKERS; i++) ran += atomic_load(&ticks[i]) > 0;
	printf("%d of %d other threads ran\n", ran, WORKERS);
	return ok == SPAWNS ? 0 : 1;
}

