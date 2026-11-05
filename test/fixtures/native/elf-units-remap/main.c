#define _GNU_SOURCE
/* One library unloaded and another loaded at the same address: each must run its own code. */
#include <dlfcn.h>
#include <link.h>
#include <stdio.h>
#include <stdlib.h>
static unsigned long base_of(void *h) { struct link_map *m = 0; dlinfo(h, RTLD_DI_LINKMAP, &m); return m ? m->l_addr : 0; }
int main(int argc, char **argv) {
	unsigned long first = 0;
	setvbuf(stdout, NULL, _IOLBF, 0);
	for (int round = 0; round < 2; round++) {
		for (const char *n = "a"; n; n = (*n == 'a' ? "b" : 0)) {
			char path[4096];
			snprintf(path, sizeof path, "%s/lib%s.so", argc > 1 ? argv[1] : ".", n);
			void *h = dlopen(path, RTLD_NOW | RTLD_LOCAL);
			if (!h) { printf("dlopen %s: %s\n", path, dlerror()); return 1; }
			int (*f)(int) = (int (*)(int))dlsym(h, "f");
			unsigned long b = base_of(h);
			if (!first) first = b;
			printf("lib%s f(5)=%d f(9)=%d %s\n", n, f(5), f(9), b == first ? "same-base" : "other-base");
			if (getenv("BASES")) fprintf(stderr, "base lib%s %#lx\n", n, b);
			dlclose(h);
		}
	}
	return 0;
}
