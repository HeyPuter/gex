#define _GNU_SOURCE
/* A library with a unit unloaded and another loaded at its base while a
   second thread (its own Worker in the browser) sleeps in a read; then that
   thread calls the new library's f through libcall's unit. The second is
   loaded from the first's path (lib.so, a copy of liba.so, then of libb.so),
   which a process maps at the address it gave that path. The unloaded
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
static int copy(const char *from, const char *to) {
	char buf[65536];
	ssize_t n;
	FILE *in = fopen(from, "rb"), *out = fopen(to, "wb");
	if (!in || !out) return -1;
	while ((n = fread(buf, 1, sizeof buf, in)) > 0)
		if (fwrite(buf, 1, n, out) != (size_t)n) return -1;
	fclose(in);
	return fclose(out);
}
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
	char lib[4096];
	snprintf(lib, sizeof lib, "%s/lib.so", dir);
	snprintf(path, sizeof path, "%s/liba.so", dir);
	if (copy(path, lib)) { printf("copy %s: failed\n", path); return 1; }
	void *ha = dlopen(lib, RTLD_NOW | RTLD_LOCAL);
	if (!ha) { printf("dlopen %s: %s\n", lib, dlerror()); return 1; }
	fp = (int (*)(int))dlsym(ha, "f");
	unsigned long a = base_of(ha);
	void *fa = (void *)fp;
	fprintf(stderr, "base liba %#lx\n", a);
	printf("main: liba f(5)=%d\n", call_f(fp, 5));
	if (pthread_create(&t, 0, thread, 0)) return 1;
	if (read(back[0], &c, 1) != 1) return 1;
	dlclose(ha);
	snprintf(path, sizeof path, "%s/libb.so", dir);
	if (copy(path, lib)) { printf("copy %s: failed\n", path); return 1; }
	void *hb = dlopen(lib, RTLD_NOW | RTLD_LOCAL);
	if (!hb) { printf("dlopen %s: %s\n", lib, dlerror()); return 1; }
	fp = (int (*)(int))dlsym(hb, "f");
	printf("libb %s, f %s\n", base_of(hb) == a ? "same-base" : "other-base", (void *)fp == fa ? "same-address" : "other-address");
	if (write(go[1], "x", 1) != 1) return 1;
	pthread_join(t, 0);
	printf("main: libb f(5)=%d\n", call_f(fp, 5));
	return 0;
}
