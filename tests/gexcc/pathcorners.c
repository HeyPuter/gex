/* Path corners gnulib checks: a trailing slash on a non-directory is
 * ENOTDIR, and rename to or from "dir/." or "dir/.." is refused. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define T(e) printf("%-48s %s\n", #e, (e) ? "ok" : strerror(errno))

int main(void)
{
	struct stat st;
	system("rm -rf pc.d pc.f; mkdir pc.d; touch pc.f");
	T(stat("pc.f/", &st) == -1 && errno == ENOTDIR);
	T(open("pc.f/", O_RDONLY) == -1 && errno == ENOTDIR);
	T(stat("pc.d/", &st) == 0);
	T(rename("pc.f", "pc.d/.") == -1);
	T(rename("pc.d/..", "pc.x") == -1);
	T(realpath("pc.f/", NULL) == NULL && errno == ENOTDIR);
	T(rename("pc.f", "pc.d/f") == 0);
	system("rm -rf pc.d pc.f");
	return 0;
}
