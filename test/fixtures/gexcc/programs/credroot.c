/* gexcc-test: as root
 * A root process's ids, capabilities and root directory, as dbus-daemon,
 * polkitd and setpriv see them. Run as root (the runners start both sides
 * in a user namespace with root and 65535 other ids mapped: unshare
 * --map-auto --map-root-user), everything in this one process and its
 * children. One line per check, compared with native.
 *  1 each of setuid, setresuid, setreuid (effective, real, both) and
 *    setresuid of three ids in a child: getresuid and getresgid after it,
 *    the capability sets it is left with, and the uid and gid the
 *    listening parent's SO_PEERCRED gives for the child's connection, and
 *    the Uid:, Gid: and Cap* lines of its /proc/<pid>/status and of the
 *    child's own /proc/self/status; the Pid: line of the
 *    /proc/self/fdinfo of a pidfd for the child (polkit's pkcheck --process
 *    reads both)
 *  2 the same after an execve of the program (setuid and setgid, then
 *    exec): the ids are kept and the far end sees them; SO_PASSCRED's
 *    SCM_CREDENTIALS gives them too; a socketpair made after setuid
 *  3 capget and capset: root's full sets, an effective capability dropped
 *    and taken back, a permitted one added refused, the version probe, the
 *    bounding set's size, PR_SET_KEEPCAPS across setuid
 *  4 chroot: absolute paths, an absolute symlink and .. resolve inside the
 *    new root, getcwd from inside it, a fork's child keeps it, a unix
 *    socket bound inside it connects; refused (EPERM) after setuid */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/capability.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif

static char sock_path[108];
/* What the parent learns of a child, printed once the child has ended: the
 * two processes' lines in one order wherever their output goes. */
static char said[4096];
#define SAY(...) snprintf(said + strlen(said), sizeof said - strlen(said), __VA_ARGS__)

static int capget3(unsigned long long *eff, unsigned long long *perm, unsigned long long *inh) {
	struct __user_cap_header_struct h = {_LINUX_CAPABILITY_VERSION_3, 0};
	struct __user_cap_data_struct d[2];
	memset(d, 0xab, sizeof d);
	int r = syscall(SYS_capget, &h, d);
	*eff = d[0].effective | (unsigned long long)d[1].effective << 32;
	*perm = d[0].permitted | (unsigned long long)d[1].permitted << 32;
	*inh = d[0].inheritable | (unsigned long long)d[1].inheritable << 32;
	return r;
}

static int capset3(unsigned long long eff, unsigned long long perm, unsigned long long inh) {
	struct __user_cap_header_struct h = {_LINUX_CAPABILITY_VERSION_3, 0};
	struct __user_cap_data_struct d[2] = {
		{(unsigned)eff, (unsigned)perm, (unsigned)inh},
		{(unsigned)(eff >> 32), (unsigned)(perm >> 32), (unsigned)(inh >> 32)},
	};
	return syscall(SYS_capset, &h, d);
}

/* The Uid:, Gid: and Cap*: lines of /proc/<pid>/status, as polkit reads
 * them for a process subject (pkcheck --process), on one line. */
static void status_lines(const char *pid, char *out, size_t room) {
	char p[64], line[256];
	snprintf(p, sizeof p, "/proc/%s/status", pid);
	FILE *f = fopen(p, "r");
	out[0] = 0;
	if (!f) {
		snprintf(out, room, "(no %s)", p);
		return;
	}
	while (fgets(line, sizeof line, f)) {
		if (strncmp(line, "Uid:", 4) && strncmp(line, "Gid:", 4) && strncmp(line, "Cap", 3))
			continue;
		line[strcspn(line, "\n")] = 0;
		for (char *c = line; *c; c++)
			if (*c == '\t')
				*c = ' ';
		snprintf(out + strlen(out), room - strlen(out), "%s%s", out[0] ? ", " : "", line);
	}
	fclose(f);
}

static void ids(const char *what) {
	uid_t r, e, s;
	gid_t gr, ge, gs;
	getresuid(&r, &e, &s);
	getresgid(&gr, &ge, &gs);
	unsigned long long ce, cp, ci;
	capget3(&ce, &cp, &ci);
	printf("%s: uids %u %u %u, gids %u %u %u, getuid %u geteuid %u, caps eff %llx perm %llx inh %llx\n", what, r, e, s, gr, ge,
	       gs, getuid(), geteuid(), ce, cp, ci);
	char st[512];
	status_lines("self", st, sizeof st);
	printf("  /proc/self/status: %s\n", st);
}

/* The child's side: connect to the parent's socket and send one byte. */
static int dial(void) {
	struct sockaddr_un a = {.sun_family = AF_UNIX};
	strcpy(a.sun_path, sock_path);
	int t = socket(AF_UNIX, SOCK_STREAM, 0);
	if (connect(t, (void *)&a, sizeof a) != 0)
		return printf("connect: %s\n", strerror(errno)), 2;
	char x = 'x';
	if (write(t, &x, 1) != 1)
		return 3;
	read(t, &x, 1);
	return 0;
}

/* The parent's side of one child: who SO_PEERCRED says connected, and with
 * `passcred`, what SCM_CREDENTIALS says sent the byte. */
static void answer(int l, pid_t child, int passcred, int status) {
	int x = accept(l, NULL, NULL);
	if (x < 0)
		return perror("accept");
	struct ucred c;
	socklen_t len = sizeof c;
	memset(&c, 0xab, sizeof c);
	getsockopt(x, SOL_SOCKET, SO_PEERCRED, &c, &len);
	SAY("  SO_PEERCRED: pid %s, uid %u, gid %u\n", c.pid == child ? "the child's" : "WRONG", c.uid, c.gid);
	/* and what another process reads of it (the child waits for our byte) */
	if (status) {
		char pid[16], st[512];
		snprintf(pid, sizeof pid, "%d", (int)child);
		status_lines(pid, st, sizeof st);
		SAY("  the child's /proc/<pid>/status: %s\n", st);
		int pfd = syscall(SYS_pidfd_open, child, 0);
		char fi[64], line[128];
		snprintf(fi, sizeof fi, "/proc/self/fdinfo/%d", pfd);
		FILE *f = pfd < 0 ? NULL : fopen(fi, "r");
		int got = -1, flags = 0;
		while (f && fgets(line, sizeof line, f)) {
			flags |= strncmp(line, "flags:", 6) == 0;
			if (strncmp(line, "Pid:", 4) == 0)
				got = atoi(line + 4);
		}
		SAY("  a pidfd for it: fdinfo %s, flags line %s, Pid: %s\n", f ? "read" : "MISSING", flags ? "yes" : "no",
		    got == child ? "the child's" : "WRONG");
		if (f)
			fclose(f);
		close(pfd);
	}
	char b;
	if (passcred) {
		int on = 1;
		setsockopt(x, SOL_SOCKET, SO_PASSCRED, &on, sizeof on);
		int got = 0;
		socklen_t gl = sizeof got;
		getsockopt(x, SOL_SOCKET, SO_PASSCRED, &got, &gl);
		union {
			struct cmsghdr h;
			char buf[CMSG_SPACE(sizeof(struct ucred))];
		} u;
		memset(&u, 0, sizeof u);
		struct iovec v = {&b, 1};
		struct msghdr m = {.msg_iov = &v, .msg_iovlen = 1, .msg_control = u.buf, .msg_controllen = sizeof u.buf};
		ssize_t n = recvmsg(x, &m, 0);
		struct cmsghdr *h = CMSG_FIRSTHDR(&m);
		if (n == 1 && h && h->cmsg_level == SOL_SOCKET && h->cmsg_type == SCM_CREDENTIALS) {
			struct ucred s;
			memcpy(&s, CMSG_DATA(h), sizeof s);
			SAY("  SO_PASSCRED %d: SCM_CREDENTIALS pid %s, uid %u, gid %u\n", got, s.pid == child ? "the child's" : "WRONG",
			       s.uid, s.gid);
		} else {
			SAY("  SO_PASSCRED %d: read %zd, no SCM_CREDENTIALS\n", got, n);
		}
	} else {
		read(x, &b, 1);
	}
	write(x, &b, 1);
	close(x);
}

static int listener(void) {
	struct sockaddr_un a = {.sun_family = AF_UNIX};
	strcpy(a.sun_path, sock_path);
	unlink(sock_path);
	int l = socket(AF_UNIX, SOCK_STREAM, 0);
	if (bind(l, (void *)&a, sizeof a) != 0 || listen(l, 4) != 0)
		return perror("bind/listen"), -1;
	/* (a connect needs write permission on the file, and the children
	 * are not root) */
	chmod(sock_path, 0777);
	return l;
}

static int wait_for(pid_t ch) {
	int st = 0;
	waitpid(ch, &st, 0);
	fputs(said, stdout);
	said[0] = 0;
	return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

static void change(int how) {
	switch (how) {
	case 0:
		setgid(996), setuid(996);
		break;
	case 1:
		setresuid(-1, 997, -1);
		break;
	case 2:
		setreuid(-1, 998);
		break;
	case 3:
		setreuid(995, -1);
		break;
	case 4:
		setresgid(980, 981, 982), setresuid(990, 991, 992);
		break;
	}
}

static const char *names[] = {"setuid 996", "setresuid -1 997 -1", "setreuid -1 998", "setreuid 995 -1", "setresuid 990 991 992"};

static void chroot_checks(const char *dir) {
	char root[256], p[300];
	snprintf(root, sizeof root, "%s/root", dir);
	snprintf(p, sizeof p, "%s/sub", root);
	if (chdir(p) != 0)
		return perror("chdir");
	printf("chroot: %d\n", chroot(root));
	char cwd[256] = "";
	printf("  getcwd: %s\n", getcwd(cwd, sizeof cwd) ? cwd : "(failed)");
	const char *paths[] = {"/etc/f", "/link/f", "/../../etc/f", "../etc/f", "/sub/../etc/f"};
	for (unsigned i = 0; i < sizeof paths / sizeof *paths; i++) {
		char buf[32] = "";
		int fd = open(paths[i], O_RDONLY);
		int n = fd >= 0 ? read(fd, buf, sizeof buf - 1) : -1;
		if (n > 0)
			buf[n] = 0;
		printf("  open %s: %s\n", paths[i], fd >= 0 ? buf : strerror(errno));
		if (fd >= 0)
			close(fd);
	}
	struct stat st;
	printf("  stat of the directory the root is in: %s\n", stat(dir, &st) == 0 ? "found (outside the root)" : strerror(errno));
	char l[64] = "";
	ssize_t n = readlink("/link", l, sizeof l - 1);
	printf("  readlink /link: %s\n", n > 0 ? l : strerror(errno));
	pid_t ch = fork();
	if (ch == 0) {
		int fd = open("/etc/f", O_RDONLY);
		printf("  a fork's child opens /etc/f: %s\n", fd >= 0 ? "yes" : strerror(errno));
		_exit(0);
	}
	wait_for(ch);
	strcpy(sock_path, "/s.sock");
	int ls = listener();
	ch = fork();
	if (ch == 0)
		_exit(dial());
	if (ls >= 0)
		answer(ls, ch, 0, 0);
	printf("  a socket bound inside: child exit %d\n", wait_for(ch));
	unlink("/s.sock");
}

int main(int argc, char **argv) {
	setvbuf(stdout, NULL, _IONBF, 0);
	if (argc == 3 && strcmp(argv[1], "dial") == 0) {
		/* After the execve: the ids kept, root's capabilities gone. */
		strcpy(sock_path, argv[2]);
		ids("  after execve");
		return dial();
	}
	if (getuid() != 0) {
		printf("not root\n");
		return 1;
	}
	snprintf(sock_path, sizeof sock_path, "/tmp/credroot-%d.sock", (int)getpid());
	int l = listener();
	if (l < 0)
		return 1;
	for (int how = 0; how < 5; how++) {
		pid_t ch = fork();
		if (ch == 0) {
			change(how);
			ids(names[how]);
			_exit(dial());
		}
		answer(l, ch, 0, 1);
		printf("  child exit %d\n", wait_for(ch));
	}
	/* The ids across an exec, and SCM_CREDENTIALS. */
	pid_t ch = fork();
	if (ch == 0) {
		change(0);
		execl("/proc/self/exe", "credroot", "dial", sock_path, (char *)0);
		_exit(9);
	}
	answer(l, ch, 1, 1);
	printf("  child exit %d\n", wait_for(ch));
	close(l);
	unlink(sock_path);
	ch = fork();
	if (ch == 0) {
		change(0);
		int sv[2];
		socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
		struct ucred c;
		socklen_t len = sizeof c;
		getsockopt(sv[0], SOL_SOCKET, SO_PEERCRED, &c, &len);
		printf("socketpair after setuid 996: peer pid %s, uid %u, gid %u\n", c.pid == getpid() ? "ours" : "WRONG", c.uid, c.gid);
		_exit(0);
	}
	wait_for(ch);

	unsigned long long e, p, i;
	capget3(&e, &p, &i);
	printf("caps: eff %llx perm %llx inh %llx\n", e, p, i);
	const unsigned long long net_raw = 1ull << CAP_NET_RAW;
	printf("capset without CAP_NET_RAW: %d\n", capset3(e & ~net_raw, p, i));
	unsigned long long e2, p2, i2;
	capget3(&e2, &p2, &i2);
	printf("  eff %llx perm %llx\n", e2, p2);
	printf("capset with it back: %d\n", capset3(e, p, i));
	printf("capset dropping it from permitted: %d\n", capset3(e & ~net_raw, p & ~net_raw, i));
	int r = capset3(e, p, i);
	printf("capset adding it back to permitted: %d %s\n", r, r ? strerror(errno) : "");
	struct __user_cap_header_struct h = {0x12345678, 0};
	r = syscall(SYS_capget, &h, NULL);
	printf("capget version probe: %d, version %x\n", r, h.version);
	r = prctl(PR_CAPBSET_READ, CAP_LAST_CAP, 0, 0, 0);
	printf("PR_CAPBSET_READ %d: %d\n", CAP_LAST_CAP, r);
	ch = fork();
	if (ch == 0) {
		prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0);
		printf("PR_GET_KEEPCAPS: %d\n", prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0));
		change(0);
		ids("  setuid 996 keeping caps");
		_exit(0);
	}
	wait_for(ch);

	char dir[] = "/tmp/credroot-XXXXXX";
	if (!mkdtemp(dir))
		return perror("mkdtemp"), 1;
	/* (searchable by the child that is not root: its chroot is refused
	 * for want of the capability, not of the path) */
	chmod(dir, 0755);
	char p1[300];
	snprintf(p1, sizeof p1, "%s/root", dir);
	mkdir(p1, 0755);
	snprintf(p1, sizeof p1, "%s/root/etc", dir);
	mkdir(p1, 0755);
	snprintf(p1, sizeof p1, "%s/root/sub", dir);
	mkdir(p1, 0755);
	snprintf(p1, sizeof p1, "%s/root/etc/f", dir);
	FILE *f = fopen(p1, "w");
	fputs("inside", f);
	fclose(f);
	snprintf(p1, sizeof p1, "%s/root/link", dir);
	symlink("/etc", p1);
	ch = fork();
	if (ch == 0) {
		chroot_checks(dir);
		_exit(0);
	}
	wait_for(ch);
	ch = fork();
	if (ch == 0) {
		change(0);
		snprintf(p1, sizeof p1, "%s/root", dir);
		int r = chroot(p1);
		printf("chroot after setuid 996: %d %s\n", r, r ? strerror(errno) : "");
		_exit(0);
	}
	wait_for(ch);
	const char *rm[] = {"root/etc/f", "root/link", "root/s.sock"};
	for (unsigned k = 0; k < 3; k++) {
		snprintf(p1, sizeof p1, "%s/%s", dir, rm[k]);
		unlink(p1);
	}
	const char *rd[] = {"root/etc", "root/sub", "root", ""};
	for (unsigned k = 0; k < 4; k++) {
		snprintf(p1, sizeof p1, "%s/%s", dir, rd[k]);
		rmdir(p1);
	}
	return 0;
}
