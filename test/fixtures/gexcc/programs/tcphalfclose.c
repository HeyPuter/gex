/* Loopback TCP between two sockets of one process: shutdown(SHUT_WR) gives
 * the peer end of file while the other direction still works; after a full
 * close the first write is taken and the next is EPIPE (SIGPIPE ignored),
 * as on Linux. */
#include <arpa/inet.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void)
{
	struct sockaddr_in a;
	socklen_t al = sizeof a;
	char b[16];
	int one = 1;
	signal(SIGPIPE, SIG_IGN);
	memset(&a, 0, sizeof a);
	a.sin_family = AF_INET;
	inet_pton(AF_INET, "127.0.0.1", &a.sin_addr);
	int s = socket(AF_INET, SOCK_STREAM, 0);
	setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
	bind(s, (void *)&a, sizeof a);
	listen(s, 1);
	getsockname(s, (void *)&a, &al);
	int c = socket(AF_INET, SOCK_STREAM, 0);
	printf("connect %d\n", connect(c, (void *)&a, sizeof a));
	int d = accept(s, 0, 0);
	printf("write %ld\n", (long)write(c, "ping", 4));
	printf("shutdown %d\n", shutdown(c, SHUT_WR));
	long n = read(d, b, sizeof b);
	printf("read %ld then eof %ld\n", n, (long)read(d, b, sizeof b));
	printf("reply %ld\n", (long)write(d, "pong", 4));
	printf("got %ld\n", (long)read(c, b, sizeof b));
	close(c);
	usleep(50000);
	printf("first write after close %ld\n", (long)write(d, "x", 1));
	usleep(50000);
	long w = write(d, "x", 1);
	printf("second %ld %s\n", w, w < 0 && errno == EPIPE ? "EPIPE" : "?");
	return 0;
}
