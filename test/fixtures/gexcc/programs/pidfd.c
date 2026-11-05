/* Process descriptors, as Qt's forkfd uses them to start and wait for a
 * QProcess child, and the rest of the interface: one line per check,
 * compared with native.
 *  1 waitid(P_PIDFD) on a closed descriptor is EBADF (how forkfd tells a
 *    kernel with pidfds from one without: EINVAL)
 *  2 clone(CLONE_PIDFD | CLONE_VFORK | CLONE_VM): the descriptor is
 *    close-on-exec, readable once the child exits, and waitid(P_PIDFD)
 *    reaps the child with its status; after the reap it stays readable
 *    (and Linux 6.9 on reports it hung up as well, so that is not checked)
 *  3 pidfd_open of a running child: not readable, epoll reports it once
 *    pidfd_send_signal has killed the child, waitid reports the signal
 *  4 PIDFD_NONBLOCK: waitid on a running child is EAGAIN
 *  5 a fork's child keeps a descriptor its parent opened before
 *  6 errors: pidfd_open of pid 0, of bad flags, of no process; waitid on
 *    the caller's own descriptor (not its child); read of a descriptor;
 *    pidfd_send_signal to a reaped process */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef CLONE_PIDFD
#define CLONE_PIDFD 0x1000
#endif
#ifndef P_PIDFD
#define P_PIDFD 3
#endif
#define NR_PIDFD_SEND_SIGNAL 424
#define NR_PIDFD_OPEN 434

static int pidfd_open(pid_t pid, unsigned flags) { return (int)syscall(NR_PIDFD_OPEN, pid, flags); }
static int pidfd_send_signal(int fd, int sig) { return (int)syscall(NR_PIDFD_SEND_SIGNAL, fd, sig, NULL, 0); }

/* errno by name: musl's and glibc's strerror texts differ */
static const char *ename(int e) {
	switch (e) {
	case EBADF: return "EBADF";
	case EINVAL: return "EINVAL";
	case ESRCH: return "ESRCH";
	case EAGAIN: return "EAGAIN";
	case ECHILD: return "ECHILD";
	case ENOSYS: return "ENOSYS";
	default: return "another errno";
	}
}
static const char *err(int r) { return r < 0 ? ename(errno) : "ok"; }

static int ready(int fd, int ms) {
	struct pollfd p = {.fd = fd, .events = POLLIN};
	int n = poll(&p, 1, ms);
	if (n < 0)
		return -1;
	return p.revents;
}

static int child_fn(void *arg) { _exit(7); }

int main(void) {
	setvbuf(stdout, NULL, _IONBF, 0);
	siginfo_t si;
	int r;

	r = waitid(P_PIDFD, INT_MAX, &si, WEXITED | WNOHANG);
	printf("1 waitid(P_PIDFD) on a closed descriptor: %s\n", err(r));

	static char stack[65536] __attribute__((aligned(16)));
	int pfd = -1;
	pid_t c = clone(child_fn, stack + sizeof stack, CLONE_PIDFD | CLONE_VFORK | CLONE_VM | SIGCHLD, NULL, &pfd);
	printf("2 clone: pid %s, descriptor %s, close-on-exec %s\n", c > 0 ? "ok" : ename(errno), pfd >= 0 ? "ok" : "none",
	       pfd >= 0 && (fcntl(pfd, F_GETFD) & FD_CLOEXEC) ? "yes" : "no");
	int ev = ready(pfd, 5000);
	printf("2 after the exit: %s\n", ev & POLLIN ? "readable" : "not readable");
	memset(&si, 0, sizeof si);
	r = waitid(P_PIDFD, pfd, &si, WEXITED);
	printf("2 waitid: %s, pid %s, code %s, status %d\n", err(r), si.si_pid == c ? "the child's" : "WRONG",
	       si.si_code == CLD_EXITED ? "exited" : "WRONG", si.si_status);
	ev = ready(pfd, 0);
	printf("2 after the reap: %s\n", ev & POLLIN ? "readable" : "not readable");
	r = pidfd_send_signal(pfd, SIGTERM);
	printf("6 pidfd_send_signal to a reaped process: %s\n", err(r));
	char b[8];
	r = (int)read(pfd, b, sizeof b);
	printf("6 read of a descriptor: %s\n", err(r));
	close(pfd);

	int pp[2];
	if (pipe(pp) != 0)
		return perror("pipe"), 1;
	c = fork();
	if (c == 0) {
		char x;
		close(pp[1]);
		read(pp[0], &x, 1);
		_exit(0);
	}
	close(pp[0]);
	pfd = pidfd_open(c, 0);
	printf("3 pidfd_open: %s, close-on-exec %s\n", pfd >= 0 ? "ok" : ename(errno), (fcntl(pfd, F_GETFD) & FD_CLOEXEC) ? "yes" : "no");
	ev = ready(pfd, 0);
	printf("3 running: %s\n", ev & POLLIN ? "readable" : "not readable");
	int nb = pidfd_open(c, O_NONBLOCK);
	r = waitid(P_PIDFD, nb, &si, WEXITED);
	printf("4 PIDFD_NONBLOCK waitid on a running child: %s\n", err(r));
	close(nb);
	pid_t g = fork();
	if (g == 0)
		_exit(fcntl(pfd, F_GETFD) >= 0 ? 0 : 1);
	int st = 0;
	waitpid(g, &st, 0);
	printf("5 a fork's child has the descriptor: %s\n", WIFEXITED(st) && WEXITSTATUS(st) == 0 ? "yes" : "no");
	int ep = epoll_create1(EPOLL_CLOEXEC);
	struct epoll_event e = {.events = EPOLLIN, .data.fd = pfd};
	r = epoll_ctl(ep, EPOLL_CTL_ADD, pfd, &e);
	printf("3 epoll_ctl: %s\n", err(r));
	struct epoll_event out;
	int n = epoll_wait(ep, &out, 1, 0);
	printf("3 epoll before the signal: %d\n", n);
	r = pidfd_send_signal(pfd, SIGTERM);
	printf("3 pidfd_send_signal: %s\n", err(r));
	n = epoll_wait(ep, &out, 1, 5000);
	printf("3 epoll after: %d%s\n", n, n == 1 && out.data.fd == pfd && (out.events & EPOLLIN) ? " (the descriptor, readable)" : "");
	memset(&si, 0, sizeof si);
	r = waitid(P_PIDFD, pfd, &si, WEXITED);
	printf("3 waitid: %s, code %s, signal %d\n", err(r), si.si_code == CLD_KILLED ? "killed" : "WRONG", si.si_status);
	close(ep);
	close(pfd);
	close(pp[1]);

	r = pidfd_open(0, 0);
	printf("6 pidfd_open(0): %s\n", err(r));
	r = pidfd_open(getpid(), 0x40);
	printf("6 pidfd_open with a bad flag: %s\n", err(r));
	r = pidfd_open(4000000, 0);
	printf("6 pidfd_open of no process: %s\n", err(r));
	int self = pidfd_open(getpid(), 0);
	r = waitid(P_PIDFD, self, &si, WEXITED | WNOHANG);
	printf("6 waitid on its own descriptor: %s\n", err(r));
	close(self);
	return 0;
}
