/* Records when it ran (CLOCK_REALTIME, ns) to the log named in argv[1],
 * holding for argv[2] milliseconds: tests/gexcc/slots/run.sh counts how
 * many overlapped. */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long long now(void)
{
	struct timespec t;
	clock_gettime(CLOCK_REALTIME, &t);
	return (long long)t.tv_sec * 1000000000 + t.tv_nsec;
}

int main(int argc, char **argv)
{
	long long a = now();
	long ms = atol(argv[2]);
	struct timespec d = {ms / 1000, (ms % 1000) * 1000000L};
	nanosleep(&d, 0);
	long long b = now();
	char line[64];
	int n = snprintf(line, sizeof line, "%lld %lld\n", a, b);
	int fd = open(argv[1], O_WRONLY | O_APPEND | O_CREAT, 0644);
	write(fd, line, n);
	close(fd);
	return 0;
}
