/* A program that links -lcrypt for a function musl's crypt lacks. */
#include <stdio.h>
const char *crypt_gensalt(const char *, unsigned long, const char *, int);
int main(void)
{
	printf("gensalt %s\n", crypt_gensalt("$y$", 0, 0, 0));
	return 0;
}
