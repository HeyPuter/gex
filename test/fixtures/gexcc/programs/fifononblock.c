/* A host FIFO with no writer, opened O_NONBLOCK: the open returns at once
 * (grep -D skip opens devices so, to fstat and skip them). */
#include <fcntl.h>
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	struct stat st;
	int fd = open(argc > 1 ? argv[1] : "fifo", O_RDONLY | O_NONBLOCK);
	if (fd < 0 || fstat(fd, &st) < 0) {
		perror("open");
		return 1;
	}
	char b[8];
	printf("fifo %d, read %ld\n", S_ISFIFO(st.st_mode), (long)read(fd, b, sizeof b));
	return 0;
}
