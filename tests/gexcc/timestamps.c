/* Timestamps to the nanosecond: utimensat and futimens set both times,
 * stat reports atime, mtime and ctime with their nanoseconds (make, tar,
 * cp -p and gnulib's stat-time rely on them). */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void)
{
	struct stat st;
	struct timespec ts[2] = {{1000000000, 123456789}, {1500000000, 987654321}};
	unlink("ts.tmp");
	int fd = open("ts.tmp", O_CREAT | O_WRONLY, 0644);
	printf("utimensat %d\n", utimensat(AT_FDCWD, "ts.tmp", ts, 0));
	stat("ts.tmp", &st);
	printf("a %ld.%09ld m %ld.%09ld\n", (long)st.st_atim.tv_sec, st.st_atim.tv_nsec, (long)st.st_mtim.tv_sec, st.st_mtim.tv_nsec);
	ts[0].tv_nsec = UTIME_OMIT;
	ts[1].tv_sec = 1600000000; ts[1].tv_nsec = 5;
	printf("futimens %d\n", futimens(fd, ts));
	fstat(fd, &st);
	printf("a %ld.%09ld m %ld.%09ld\n", (long)st.st_atim.tv_sec, st.st_atim.tv_nsec, (long)st.st_mtim.tv_sec, st.st_mtim.tv_nsec);
	printf("ctime after mtime set: %d\n", st.st_ctim.tv_sec > 1600000000);
	ts[1].tv_nsec = 1000000000;
	printf("bad nsec %d\n", utimensat(AT_FDCWD, "ts.tmp", ts, 0));
	close(fd);
	unlink("ts.tmp");
	return 0;
}
