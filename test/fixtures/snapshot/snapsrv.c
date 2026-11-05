/* A session snapshot with wineserver in it: starts `wineserver -f -p`
 * (WINESERVER names it) on a new prefix, connects to the server's socket
 * once it is there (the server answers a new client with its protocol
 * version and a descriptor), says "parked", sleeps, connects again, then
 * ends the server with SIGTERM and says how it ended. Snapshotted at
 * "parked" and restored, the second client is the restored server's
 * (test/snapshot/snapshot). */
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <dirent.h>
#include <time.h>
#include <unistd.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/un.h>

extern char **environ;

static char sock[512];

static int has_socket(const char *dir)
{
	DIR *d = opendir(dir);
	if (!d) return 0;
	struct dirent *e;
	int found = 0;
	while (!found && (e = readdir(d))) {
		if (strncmp(e->d_name, "server-", 7)) continue;
		snprintf(sock, sizeof sock, "%s/%s/socket", dir, e->d_name);
		struct stat st;
		found = !stat(sock, &st);
	}
	closedir(d);
	return found;
}

/* A client's first exchange: the server's protocol version, and whether a
 * descriptor came with it. */
static void client(const char *when)
{
	int s = socket(AF_UNIX, SOCK_STREAM, 0);
	struct sockaddr_un a = { .sun_family = AF_UNIX };
	strncpy(a.sun_path, sock, sizeof a.sun_path - 1);
	if (connect(s, (struct sockaddr *)&a, sizeof a)) { printf("%s: connect failed\n", when); close(s); return; }
	int version = 0;
	char cbuf[CMSG_SPACE(sizeof(int))];
	struct iovec iov = { &version, sizeof version };
	struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf };
	ssize_t n = recvmsg(s, &m, 0);
	struct cmsghdr *c = CMSG_FIRSTHDR(&m);
	int fd = c && c->cmsg_type == SCM_RIGHTS ? *(int *)CMSG_DATA(c) : -1;
	printf("%s: %zd bytes, protocol %d, %s\n", when, n, version, fd >= 0 ? "a descriptor" : "no descriptor");
	if (fd >= 0) close(fd);
	close(s);
}

int main(void)
{
	const char *srv = getenv("WINESERVER");
	if (!srv) { puts("set WINESERVER"); return 2; }
	mkdir("/tmp/snapsrv", 0700);
	setenv("WINEPREFIX", "/tmp/snapsrv", 1);
	char dir[64];
	snprintf(dir, sizeof dir, "/tmp/.wine-%u", (unsigned)getuid());
	pid_t s;
	char *sa[] = { (char *)srv, "-f", "-p", 0 };
	if (posix_spawn(&s, srv, 0, 0, sa, environ)) { puts("spawn failed"); return 1; }
	for (int i = 0; i < 2000 && !has_socket(dir); i++) {
		struct timespec t = { 0, 10000000 };
		nanosleep(&t, 0);
	}
	if (!has_socket(dir)) { puts("no server socket"); return 1; }
	client("first client");
	printf("parked\n");
	fflush(stdout);
	struct timespec ts = { 1, 500000000 };
	nanosleep(&ts, 0);
	client("second client");
	kill(s, SIGTERM);
	int ss;
	waitpid(s, &ss, 0);
	printf("wineserver: %s %d\n", WIFEXITED(ss) ? "exit" : "signal", WIFEXITED(ss) ? WEXITSTATUS(ss) : WTERMSIG(ss));
	return 0;
}
