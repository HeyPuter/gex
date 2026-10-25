#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
long co_call(long (*cb)(long), long arg);
/* Large values in the low argument registers, where x86's stack pointer
   sits in the context. */
__attribute__((noinline)) static long cb(long i)
{
	register long x4 __asm__("x4") = 0x7ffffff0000 + i;
	__asm__ volatile("" : "+r"(x4));
	return i * 10;
}
int main(int argc, char **argv)
{
	long n = argc > 1 ? atol(argv[1]) : 40;
	int bad = 0;
	setvbuf(stdout, NULL, _IOLBF, 0);
	for (long i = 0; i < n; i++) {
		long r = co_call(cb, i);
		if (r != i * 10 + 1) {
			printf("co_call %ld: %ld\n", i, r);
			bad = 1;
		}
	}
	printf("%s\n", bad ? "wrong" : "ok");
	_exit(bad);
}
