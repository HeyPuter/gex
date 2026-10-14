#define _GNU_SOURCE
/* A library with a unit unloaded and another loaded at its base while a
   second thread (its own Worker in the browser) sleeps in a read; then that
   thread calls the new library's f through libcall's unit. The unloaded
   unit's probe cache entry must not survive: the thread's table still holds
   the old unit's body, which would find its page patched and leave through
   the same entry to itself, forever (cgen::on_retire). */
#include <dlfcn.h>
#include <link.h>
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>
int call_f(int (*fp)(int), int x);
static int go[2], back[2];
static int (*volatile fp)(int);
static unsigned long base_of(void *h) { struct link_map *m = 0; dlinfo(h, RTLD_DI_LINKMAP, &m); return m ? m->l_addr : 0; }
static void *thread(void *arg) {
	char c;
	(void)arg;
	printf("thread: liba f(5)=%d\n", call_f(fp, 5));
	if (write(back[1], "x", 1) != 1) return 0;
	if (read(go[0], &c, 1) != 1) return 0;
	printf("thread: libb f(5)=%d\n", call_f(fp, 5));
	return 0;
}
int main(int argc, char **argv) {
	char path[4096];
	pthread_t t;
	char c;
	const char *dir = argc > 1 ? argv[1] : ".";
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (pipe(go) || pipe(back)) return 1;
	snprintf(path, sizeof path, "%s/liba.so", dir);
	void *ha = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!ha) { printf("dlopen %s: %s\n", path, dlerror()); return 1; }
	fp = (int (*)(int))dlsym(ha, "f");
	unsigned long a = base_of(ha);
	void *fa = (void *)fp;
	fprintf(stderr, "base liba %#lx\n", a);
	printf("main: liba f(5)=%d\n", call_f(fp, 5));
	if (pthread_create(&t, 0, thread, 0)) return 1;
	if (read(back[0], &c, 1) != 1) return 1;
	dlclose(ha);
	snprintf(path, sizeof path, "%s/libb.so", dir);
	void *hb = dlopen(path, RTLD_NOW | RTLD_LOCAL);
	if (!hb) { printf("dlopen %s: %s\n", path, dlerror()); return 1; }
	fp = (int (*)(int))dlsym(hb, "f");
	printf("libb %s, f %s\n", base_of(hb) == a ? "same-base" : "other-base", (void *)fp == fa ? "same-address" : "other-address");
	if (write(go[1], "x", 1) != 1) return 1;
	pthread_join(t, 0);
	printf("main: libb f(5)=%d\n", call_f(fp, 5));
	return 0;
}
