/* Standard streams redirected from a host file: the guest's offset is the
 * host descriptor's (stdinseek.sh reads on after the program). */
#include <stdio.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void)
{
	char b[4];
	struct stat st;
	int r = fstat(0, &st);
	long n = read(0, b, 4);
	long cur = (long)lseek(0, 0, SEEK_CUR);
	long set = (long)lseek(0, 6, SEEK_SET);
	printf("fstat %d regular %d size %lld read %ld cur %ld set %ld\n", r, S_ISREG(st.st_mode),
	       (long long)st.st_size, n, cur, set);
	return 0;
}
