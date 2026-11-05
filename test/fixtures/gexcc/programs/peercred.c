/* SO_PEERCRED, as dbus-daemon uses it to authenticate a client (EXTERNAL):
 * the far end's pid, uid and gid, every byte of the struct written. The
 * struct is filled with a pattern first, so a getsockopt that succeeds and
 * leaves bytes alone shows (dbus-daemon read its struct uninitialised and
 * refused every client). One line per check, compared with native.
 *  1 socketpair: each end's peer is the process that made the pair, also
 *    from a child the pair was handed to across a fork
 *  2 listen/connect: the accepted end's peer is the connecting process, the
 *    connecting end's peer the listening one
 *  3 the length is 12, and a shorter buffer gets that much and no more
 * The connecting child's line is the parent's to print (sent over the
 * connection): two processes' lines on one stdout keep no order.
 * Natively and in the page (a pair or connection between processes there
 * is in the global kernel, which keeps each end's credentials). */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int peer(int fd, struct ucred *c)
{
	socklen_t l = sizeof *c;
	memset(c, 0xab, sizeof *c);
	if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, c, &l) != 0)
		return -1;
	return (int)l;
}

static int line(char *o, size_t n, const char *what, int fd, pid_t want)
{
	struct ucred c;
	int l = peer(fd, &c);
	return snprintf(o, n, "%s: len %d, pid %s, uid %s, gid %s\n", what, l,
	                c.pid == want ? "the peer's" : "WRONG",
	                c.uid == getuid() ? "ours" : "WRONG",
	                c.gid == getgid() ? "ours" : "WRONG");
}

static void check(const char *what, int fd, pid_t want)
{
	char o[128];
	line(o, sizeof o, what, fd, want);
	fputs(o, stdout);
}

int main(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	int sv[2];
	if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0)
		return perror("socketpair"), 1;
	pid_t me = getpid();
	check("socketpair end 0", sv[0], me);
	check("socketpair end 1", sv[1], me);

	pid_t ch = fork();
	if (ch == 0) {
		check("socketpair end in a child", sv[1], me);
		_exit(0);
	}
	waitpid(ch, NULL, 0);

	struct ucred c;
	unsigned char buf[sizeof c + 4];
	memset(buf, 0xab, sizeof buf);
	socklen_t l = 6;
	getsockopt(sv[0], SOL_SOCKET, SO_PEERCRED, buf, &l);
	printf("a 6-byte buffer: len %u, byte 6 %s\n", (unsigned)l, buf[6] == 0xab ? "untouched" : "WRITTEN");

	struct sockaddr_un a = {.sun_family = AF_UNIX};
	snprintf(a.sun_path, sizeof a.sun_path, "/tmp/peercred-%d.sock", (int)me);
	unlink(a.sun_path);
	int s = socket(AF_UNIX, SOCK_STREAM, 0);
	if (bind(s, (void *)&a, sizeof a) != 0 || listen(s, 1) != 0)
		return perror("bind/listen"), 1;
	ch = fork();
	if (ch == 0) {
		int t = socket(AF_UNIX, SOCK_STREAM, 0);
		if (connect(t, (void *)&a, sizeof a) != 0)
			_exit(2);
		char o[128];
		int n = line(o, sizeof o, "connecting end", t, me);
		write(t, o, n);
		char x;
		read(t, &x, 1);
		_exit(0);
	}
	int x = accept(s, NULL, NULL);
	char o[128], b = 0;
	int n = 0, k;
	while (n < (int)sizeof o - 1 && (k = read(x, o + n, sizeof o - 1 - n)) > 0 && o[n + k - 1] != '\n')
		n += k;
	if (k > 0)
		n += k;
	fwrite(o, 1, n, stdout);
	check("accepted end", x, ch);
	write(x, &b, 1);
	int st;
	waitpid(ch, &st, 0);
	unlink(a.sun_path);
	printf("child exit %d\n", WEXITSTATUS(st));
	return 0;
}
