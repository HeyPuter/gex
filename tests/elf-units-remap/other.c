#define _GNU_SOURCE
/* two.c's other program: maps liba.so once told to. */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
int main(int argc, char **argv) {
	char path[4096], g;
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (argc < 3 || read(atoi(argv[2]), &g, 1) != 1) return 2;
	snprintf(path, sizeof path, "%s/liba.so", argv[1]);
	void *h = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!h) { printf("dlopen %s: %s\n", path, dlerror()); return 1; }
	int (*f)(int) = (int (*)(int))dlsym(h, "f");
	printf("other f(7)=%d\n", f(7));
	return 0;
}
