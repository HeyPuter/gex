/* What a fork's or a posix_spawn's child gets and what its parent keeps,
 * and every way such a child ends; natively, the same calls. Each part
 * prints what the parent and the child see.
 *
 *  1. the child changes its signal mask and dispositions, umask, working
 *     directory, environment and descriptors, then execs this program to
 *     report what it was given; the parent's own view is checked after
 *     (mask, dispositions, umask, cwd, environment, a descriptor and its
 *     offset, an epoll set's registration);
 *  2. the child arms a timer and leaves a signal pending: the parent has
 *     neither;
 *  3. the child blocks on a pipe the parent writes after the fork;
 *  4. the child forks again;
 *  5. the child makes many calls;
 *  6. the child decompresses a file into a pipe, as dpkg-deb's does;
 *  7. the parent is sent a signal while its child runs, and takes it;
 *  8. a parent with a second thread forks;
 *  9. the child crashes (a store far outside its memory): the parent goes on;
 * 10. posix_spawn with file actions and attributes (gex runs the child in
 *     the parent's Worker until its exec: shims/browser/src/vfork.rs);
 * 11. posix_spawn of a program that is not there: ENOENT, through the
 *     child's error pipe;
 * 12. posix_spawn of a program that exits at once, many times. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <libgen.h>
#include <pthread.h>
#include <spawn.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;
static char self[4096];
static volatile sig_atomic_t usr1;

static void on_usr1(int s) { usr1 = s; }

static int status_of(pid_t pid)
{
	int st;
	pid_t r;
	/* gex ends a wait with EINTR even under SA_RESTART (signal() sets it) */
	while ((r = waitpid(pid, &st, 0)) < 0 && errno == EINTR)
		;
	if (r != pid) return -1;
	return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

static const char *disp(int s)
{
	struct sigaction sa;
	sigaction(s, 0, &sa);
	return sa.sa_handler == SIG_IGN ? "ignored" : sa.sa_handler == SIG_DFL ? "default" : "handled";
}

static int blocked(int s)
{
	sigset_t m;
	sigprocmask(SIG_SETMASK, 0, &m);
	return sigismember(&m, s);
}

/* The exec'd program's half of part 1. */
static int report(void)
{
	char buf[16] = {0}, cwd[4096];
	mode_t um = umask(0);
	printf("exec: SIGTERM blocked %d, SIGUSR2 blocked %d\n", blocked(SIGTERM), blocked(SIGUSR2));
	printf("exec: SIGINT %s, SIGPIPE %s, SIGUSR1 %s\n", disp(SIGINT), disp(SIGPIPE), disp(SIGUSR1));
	printf("exec: umask %03o\n", (unsigned)um);
	const char *start = getenv("IP_START");
	printf("exec: cwd is the start's parent %d\n",
	       start && getcwd(cwd, sizeof cwd) && strcmp(dirname(strdup(start)), cwd) == 0);
	printf("exec: FOO=%s BAR=%s\n", getenv("FOO") ? getenv("FOO") : "-", getenv("BAR") ? getenv("BAR") : "-");
	ssize_t n = pread(9, buf, 7, 3);
	printf("exec: fd 9 reads %zd \"%s\", at %ld\n", n, buf, (long)lseek(9, 0, SEEK_CUR));
	printf("exec: fd 8 (close-on-exec) open %d, fd 7 open %d\n", fcntl(8, F_GETFD) >= 0, fcntl(7, F_GETFD) >= 0);
	return 0;
}

static void part1(void)
{
	char cwd[4096], again[4096];
	umask(022);
	sigset_t m;
	sigemptyset(&m);
	sigaddset(&m, SIGUSR2);
	sigprocmask(SIG_BLOCK, &m, 0);
	signal(SIGINT, SIG_DFL); /* a shell's background job starts with it ignored */
	signal(SIGPIPE, SIG_IGN);
	signal(SIGUSR1, on_usr1);
	setenv("FOO", "parent", 1);
	int fd = open("ip.data", O_RDWR | O_CREAT | O_TRUNC, 0644);
	write(fd, "0123456789", 10);
	lseek(fd, 3, SEEK_SET);
	int p[2];
	pipe(p);
	int ep = epoll_create1(0);
	struct epoll_event ev = { .events = EPOLLIN, .data.fd = p[0] };
	epoll_ctl(ep, EPOLL_CTL_ADD, p[0], &ev);
	getcwd(cwd, sizeof cwd);
	setenv("IP_START", cwd, 1);
	pid_t pid = fork();
	if (pid == 0) {
		sigset_t c;
		sigemptyset(&c);
		sigaddset(&c, SIGTERM);
		sigprocmask(SIG_BLOCK, &c, 0);
		sigemptyset(&c);
		sigaddset(&c, SIGUSR2);
		sigprocmask(SIG_UNBLOCK, &c, 0);
		signal(SIGINT, SIG_IGN);
		signal(SIGPIPE, SIG_DFL);
		umask(077);
		chdir("..");
		setenv("FOO", "child", 1);
		setenv("BAR", "new", 1);
		dup2(fd, 9);
		close(fd);
		char b[4];
		pread(9, b, 4, 0);
		int k = open(self, O_RDONLY);
		dup2(k, 8);
		fcntl(8, F_SETFD, FD_CLOEXEC);
		dup2(k, 7);
		close(k);
		execl(self, self, "report", (char *)0);
		_exit(127);
	}
	printf("part 1: child exit %d\n", status_of(pid));
	printf("parent: SIGTERM blocked %d, SIGUSR2 blocked %d\n", blocked(SIGTERM), blocked(SIGUSR2));
	printf("parent: SIGINT %s, SIGPIPE %s, SIGUSR1 %s\n", disp(SIGINT), disp(SIGPIPE), disp(SIGUSR1));
	mode_t um = umask(022);
	printf("parent: umask %03o, cwd same %d, FOO=%s BAR=%s\n", (unsigned)um,
	       getcwd(again, sizeof again) && strcmp(cwd, again) == 0, getenv("FOO"), getenv("BAR") ? getenv("BAR") : "-");
	char buf[8] = {0};
	long at = (long)lseek(fd, 0, SEEK_CUR);
	int got4 = (int)read(fd, buf, 4);
	printf("parent: fd at %ld, reads \"%.*s\"", at, got4, buf);
	printf(", fd 9 open %d\n", fcntl(9, F_GETFD) >= 0);
	write(p[1], "x", 1);
	struct epoll_event got;
	printf("parent: epoll sees the pipe %d\n", epoll_wait(ep, &got, 1, 1000) == 1 && got.data.fd == p[0]);
	close(fd);
	close(p[0]);
	close(p[1]);
	close(ep);
}

static void part2(void)
{
	pid_t pid = fork();
	if (pid == 0) {
		alarm(100);
		sigset_t m;
		sigemptyset(&m);
		sigaddset(&m, SIGUSR1);
		sigprocmask(SIG_BLOCK, &m, 0);
		raise(SIGUSR1);
		sigset_t pend;
		sigpending(&pend);
		_exit(sigismember(&pend, SIGUSR1) ? 3 : 4);
	}
	printf("part 2: child exit %d\n", status_of(pid));
	sigset_t pend;
	sigpending(&pend);
	printf("parent: alarm left %u, SIGUSR1 pending %d, SIGUSR1 blocked %d\n", alarm(0), sigismember(&pend, SIGUSR1),
	       blocked(SIGUSR1));
}

static void part3(void)
{
	int p[2];
	pipe(p);
	pid_t pid = fork();
	if (pid == 0) {
		close(p[1]);
		char b[16] = {0};
		ssize_t n = read(p[0], b, sizeof b - 1);
		_exit(n == 5 && strcmp(b, "hello") == 0 ? 5 : 1);
	}
	close(p[0]);
	write(p[1], "hello", 5);
	close(p[1]);
	printf("part 3: child exit %d\n", status_of(pid));
}

static void part4(void)
{
	pid_t pid = fork();
	if (pid == 0) {
		pid_t g = fork();
		if (g == 0) _exit(40);
		_exit(status_of(g) + 1);
	}
	printf("part 4: child exit %d\n", status_of(pid));
}

static void part5(void)
{
	pid_t pid = fork();
	if (pid == 0) {
		unsigned acc = 0;
		for (int i = 0; i < 10000; i++) acc += umask(022);
		_exit(acc == 10000u * 022 ? 6 : 1);
	}
	printf("part 5: child exit %d\n", status_of(pid));
}

static void part6(void)
{
	int fd = open("ip.src", O_RDWR | O_CREAT | O_TRUNC, 0644);
	/* "compressed": (count, byte) pairs */
	unsigned char rle[64];
	for (int i = 0; i < 32; i++) {
		rle[2 * i] = (unsigned char)(i * 7 + 1);
		rle[2 * i + 1] = (unsigned char)('a' + i % 26);
	}
	write(fd, rle, sizeof rle);
	close(fd);
	int p[2];
	pipe(p);
	pid_t pid = fork();
	if (pid == 0) {
		close(p[0]);
		int in = open("ip.src", O_RDONLY);
		unsigned char pair[2], out[256];
		while (read(in, pair, 2) == 2) {
			memset(out, pair[1], pair[0]);
			if (write(p[1], out, pair[0]) != pair[0]) _exit(1);
		}
		_exit(0);
	}
	close(p[1]);
	unsigned sum = 0, n = 0;
	unsigned char b[512];
	ssize_t k;
	while ((k = read(p[0], b, sizeof b)) > 0) {
		for (ssize_t i = 0; i < k; i++) sum = sum * 31 + b[i];
		n += k;
	}
	close(p[0]);
	printf("part 6: child exit %d, %u bytes, sum %u\n", status_of(pid), n, sum);
}

static void part7(void)
{
	unlink("ip.flag");
	unlink("ip.go");
	usr1 = 0;
	signal(SIGUSR1, on_usr1);
	sigset_t m;
	sigemptyset(&m);
	sigaddset(&m, SIGUSR1);
	sigprocmask(SIG_UNBLOCK, &m, 0);
	int ready[2];
	pipe(ready);
	pid_t parent = getpid();
	pid_t h = fork();
	if (h == 0) {
		/* running, said through a pipe the parent waits on */
		write(ready[1], "r", 1);
		struct stat st;
		struct timespec ms = { 0, 1000000 };
		for (int i = 0; i < 20000 && stat("ip.flag", &st) != 0; i++) nanosleep(&ms, 0);
		kill(parent, SIGUSR1);
		close(open("ip.go", O_WRONLY | O_CREAT, 0644));
		_exit(0);
	}
	char r;
	read(ready[0], &r, 1);
	pid_t c = fork();
	if (c == 0) {
		close(open("ip.flag", O_WRONLY | O_CREAT, 0644));
		struct stat st;
		struct timespec t0, t;
		clock_gettime(CLOCK_MONOTONIC, &t0);
		/* until the helper has sent the signal, or ten seconds */
		while (stat("ip.go", &st) != 0) {
			clock_gettime(CLOCK_MONOTONIC, &t);
			if (t.tv_sec > t0.tv_sec + 10) _exit(2);
		}
		_exit(usr1 ? 1 : 0);
	}
	int cs = status_of(c), hs = status_of(h);
	printf("part 7: child exit %d, helper exit %d, parent took SIGUSR1 %d\n", cs, hs, usr1 == SIGUSR1);
	close(ready[0]);
	close(ready[1]);
}

static void part9(void)
{
	pid_t pid = fork();
	if (pid == 0) {
		*(volatile int *)(~(unsigned long)0 - 7) = 1;
		_exit(1);
	}
	/* by a signal: SIGSEGV natively, SIGKILL from gex (a trapped Worker) */
	printf("part 9: child killed by a signal %d\n", status_of(pid) > 128);
}

static void *waiter(void *arg)
{
	char b;
	read(*(int *)arg, &b, 1);
	return 0;
}

static void part8(void)
{
	int p[2];
	pipe(p);
	pthread_t t;
	pthread_create(&t, 0, waiter, &p[0]);
	signal(SIGINT, SIG_IGN);
	pid_t pid = fork();
	if (pid == 0) {
		_exit(strcmp(disp(SIGINT), "ignored") == 0 ? 8 : 1);
	}
	printf("part 8: child exit %d\n", status_of(pid));
	write(p[1], "x", 1);
	pthread_join(t, 0);
	signal(SIGINT, SIG_DFL);
}

/* posix_spawn's child, exec'd: what part 10's attributes and actions
 * gave it. */
static int spawned(void)
{
	char buf[8] = {0};
	printf("spawned: SIGTERM blocked %d, SIGUSR2 blocked %d, SIGPIPE %s, SIGINT %s\n",
	       blocked(SIGTERM), blocked(SIGUSR2), disp(SIGPIPE), disp(SIGINT));
	printf("spawned: own group %d, FOO=%s\n", getpgid(0) == getpid(), getenv("FOO") ? getenv("FOO") : "-");
	printf("spawned: fd 5 reads %zd \"%s\", fd 6 open %d\n", pread(5, buf, 4, 0), buf, fcntl(6, F_GETFD) >= 0);
	return 4;
}

static void part10(void)
{
	sigset_t mask, def;
	sigemptyset(&mask);
	sigaddset(&mask, SIGTERM);
	sigemptyset(&def);
	sigaddset(&def, SIGPIPE);
	signal(SIGPIPE, SIG_IGN);
	signal(SIGINT, SIG_IGN);
	posix_spawnattr_t at;
	posix_spawnattr_init(&at);
	posix_spawnattr_setflags(&at, POSIX_SPAWN_SETSIGMASK | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETPGROUP);
	posix_spawnattr_setsigmask(&at, &mask);
	posix_spawnattr_setsigdefault(&at, &def);
	posix_spawnattr_setpgroup(&at, 0);
	int fd = open("ip.data", O_RDWR | O_CREAT | O_TRUNC, 0644);
	write(fd, "abcdefgh", 8);
	int keep = open(self, O_RDONLY);
	dup2(keep, 6);
	close(keep);
	posix_spawn_file_actions_t fa;
	posix_spawn_file_actions_init(&fa);
	posix_spawn_file_actions_adddup2(&fa, fd, 5);
	posix_spawn_file_actions_addclose(&fa, 6);
	setenv("FOO", "spawn", 1);
	char *av[] = { self, "spawned", 0 };
	pid_t pid;
	int r = posix_spawn(&pid, self, &fa, &at, av, environ);
	printf("part 10: posix_spawn %d, child exit %d\n", r, r ? -1 : status_of(pid));
	printf("parent: SIGTERM blocked %d, SIGPIPE %s, SIGINT %s, fd 6 open %d, fd 5 open %d, at %ld\n",
	       blocked(SIGTERM), disp(SIGPIPE), disp(SIGINT), fcntl(6, F_GETFD) >= 0,
	       fcntl(5, F_GETFD) >= 0, (long)lseek(fd, 0, SEEK_CUR));
	posix_spawn_file_actions_destroy(&fa);
	posix_spawnattr_destroy(&at);
	close(fd);
	close(6);
	signal(SIGPIPE, SIG_DFL);
	signal(SIGINT, SIG_DFL);
}

static void part11(void)
{
	char *av[] = { "nothere", 0 };
	pid_t pid = 0;
	int r = posix_spawn(&pid, "/nonexistent/nothere", 0, 0, av, environ);
	printf("part 11: posix_spawn %s\n", r == ENOENT ? "ENOENT" : strerror(r));
}

static void part12(void)
{
	char *av[] = { self, "quick", 0 };
	int bad = 0;
	for (int i = 0; i < 20; i++) {
		pid_t pid;
		if (posix_spawn(&pid, self, 0, 0, av, environ) != 0 || status_of(pid) != 9) bad++;
	}
	printf("part 12: 20 spawns, %d wrong\n", bad);
}

int main(int argc, char **argv)
{
	setvbuf(stdout, 0, _IONBF, 0);
	if (argc > 1 && strcmp(argv[1], "report") == 0) return report();
	if (argc > 1 && strcmp(argv[1], "spawned") == 0) return spawned();
	if (argc > 1 && strcmp(argv[1], "quick") == 0) return 9;
	ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
	if (n <= 0) return 1;
	self[n] = 0;
	part1();
	part2();
	part3();
	part4();
	part5();
	part6();
	part7();
	part9();
	part10();
	part11();
	part12();
	part8();
	unlink("ip.data");
	unlink("ip.src");
	unlink("ip.flag");
	unlink("ip.go");
	return 0;
}
