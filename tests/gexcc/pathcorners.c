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
	system("rm -rf pc.d pc.f pc.l pc.dangling pc.loop; mkdir pc.d; touch pc.f");
	T(stat("pc.f/", &st) == -1 && errno == ENOTDIR);
	T(open("pc.f/", O_RDONLY) == -1 && errno == ENOTDIR);
	T(stat("pc.d/", &st) == 0);
	T(symlink("pc.d", "pc.l") == 0);
	T(lstat("pc.l", &st) == 0 && S_ISLNK(st.st_mode));
	T(lstat("pc.l/", &st) == 0 && S_ISDIR(st.st_mode));
	T(symlink("pc.nosuch", "pc.dangling") == 0);
	T(lstat("pc.dangling/", &st) == -1 && errno == ENOENT);
	T(symlink("pc.loop", "pc.loop") == 0);
	T(lstat("pc.loop/", &st) == -1 && errno == ELOOP);
	T(rename("pc.f", "pc.d/.") == -1);
	T(rename("pc.d/..", "pc.x") == -1);
	T(realpath("pc.f/", NULL) == NULL && errno == ENOTDIR);
	T(mkdir("pc.d/.", 0755) == -1 && errno == EEXIST);
	T(mkdir("pc.none/.", 0755) == -1 && errno == ENOENT);
	T(mkdir("pc.dangling/", 0755) == -1 && errno == EEXIST);
	T(rename("pc.d", "pc.dangling/") == -1 && errno == ENOTDIR);
	T(rmdir("pc.d/.") == -1 && errno == EINVAL);
	T(link("pc.f", "pc.d/.") == -1 && errno == EEXIST);
	T(link("pc.f", "pc.new/") == -1 && errno == ENOENT);
	T(symlink("nowhere", "pc.new/") == -1 && errno == ENOENT);
	T(rename("pc.f", "pc.new/") == -1 && errno == ENOTDIR);
	T(open("pc.new/", O_CREAT | O_WRONLY, 0644) == -1 && errno == EISDIR);
	T(access("pc.new", F_OK) == -1);
	T(rename("pc.f", "pc.d/f") == 0);
	system("rm -rf pc.d pc.f pc.l pc.dangling pc.loop");
	return 0;
}
