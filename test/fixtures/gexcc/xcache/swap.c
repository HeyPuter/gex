/* A replace during a session: after the load restored, before any process
 * uses the library, put a new libt.so in place (as apt does: rename over
 * it), then run /bin/t. */
#include <stdio.h>
#include <unistd.h>
int main(void)
{
	if (rename("/lib/libt.new", "/lib/libt.so")) {
		perror("rename");
		return 1;
	}
	execl("/bin/t", "/bin/t", (char *)0);
	perror("execl");
	return 1;
}
