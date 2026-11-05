/* An empty path names nothing: every path call fails with ENOENT. The
 * AT_EMPTY_PATH forms take the descriptor instead. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void show(const char *what, int r)
{
	printf("%s: %d %s\n", what, r < 0 ? -1 : 0, r < 0 ? strerror(errno) : "");
	errno = 0;
}

int main(void)
{
	struct stat st;
	FILE *f;
	show("open", open("", O_RDONLY));
	show("stat", stat("", &st));
	show("lstat", lstat("", &st));
	show("access", access("", F_OK));
	show("fstatat", fstatat(AT_FDCWD, "", &st, 0));
	show("fstatat AT_EMPTY_PATH", fstatat(0, "", &st, AT_EMPTY_PATH));
	show("chdir", chdir(""));
	show("mkdir", mkdir("", 0755));
	show("unlink", unlink(""));
	f = fopen("", "r");
	printf("fopen: %s\n", f ? "opened" : strerror(errno));
	return 0;
}
