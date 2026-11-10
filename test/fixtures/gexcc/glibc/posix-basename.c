/* <libgen.h>'s POSIX basename wins over <string.h>'s GNU one under
 * _GNU_SOURCE, in either order, as in glibc. */
#define _GNU_SOURCE 1
#include <string.h>
#include <libgen.h>
#include <stdio.h>

int main(void)
{
	char a[] = "/a/b/";
	printf("posix [%s]\n", basename(a));
	return 0;
}
