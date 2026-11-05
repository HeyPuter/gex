/* O_DIRECTORY on what is not a directory is ENOTDIR, with O_PATH too and
 * through a symlink: coreutils' mv asks whether its last operand is a
 * directory so, and `mv a b` onto a file moved a to b/a. Then mv's own
 * steps: the probe, and rename onto the existing file. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define T(e) printf("%-64s %s\n", #e, (e) ? "ok" : strerror(errno))

static void put(const char *name, const char *text)
{
	FILE *f = fopen(name, "w");
	fputs(text, f);
	fclose(f);
}

int main(void)
{
	char buf[8] = {0};
	unlink("od.a"); unlink("od.b"); unlink("od.l"); rmdir("od.d");
	put("od.a", "a");
	put("od.b", "b");
	T(symlink("od.b", "od.l") == 0);
	T(mkdir("od.d", 0755) == 0);
	T(open("od.b", O_RDONLY | O_DIRECTORY) == -1 && errno == ENOTDIR);
	T(open("od.b", O_PATH | O_DIRECTORY) == -1 && errno == ENOTDIR);
	T(open("od.l", O_RDONLY | O_DIRECTORY) == -1 && errno == ENOTDIR);
	T(open("od.l", O_PATH | O_DIRECTORY) == -1 && errno == ENOTDIR);
	T(open("od.b", O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOCTTY) == -1 && errno == ENOTDIR);
	T(open("od.d", O_RDONLY | O_DIRECTORY) >= 0);
	T(open("od.d", O_PATH | O_DIRECTORY) >= 0);
	T(openat(AT_FDCWD, "od.b", O_RDONLY | O_DIRECTORY) == -1 && errno == ENOTDIR);
	/* mv a b: b is not a directory, so a is renamed onto it */
	T(open("od.b", O_PATH | O_DIRECTORY | O_CLOEXEC) == -1 && errno == ENOTDIR);
	T(rename("od.a", "od.b") == 0);
	int fd = open("od.b", O_RDONLY);
	T(read(fd, buf, sizeof buf - 1) == 1 && strcmp(buf, "a") == 0);
	T(access("od.a", F_OK) == -1 && errno == ENOENT);
	unlink("od.b"); unlink("od.l"); rmdir("od.d");
	return 0;
}
