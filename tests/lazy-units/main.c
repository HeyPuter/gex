/* Function-tier units a thread's Worker instantiates at their first call
   (shims/browser/js/gex.mjs `#cgenDefer`). The main thread calls libb.so's
   b_two, then rewrites it (its unit's slot retired); a thread then dlopens
   libd.so, whose constructor calls libb.so's b_mul, and calls b_two, which
   must run the new code, not the unit's body that libb.so's instantiation
   in that Worker put back. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>

int b_mul(int);
int b_two(int);

static const char *dir = ".";

static void *thread(void *arg)
{
	(void)arg;
	char path[4096];
	snprintf(path, sizeof path, "%s/libd.so", dir);
	void *h = dlopen(path, RTLD_NOW);
	if (!h) { printf("dlopen %s: %s\n", path, dlerror()); return 0; }
	int (*d_get)(void) = (int (*)(void))dlsym(h, "d_get");
	printf("thread d_get %d\n", d_get());
	printf("thread b_two %d\n", b_two(1000));
	printf("thread b_mul %d\n", b_mul(6));
	return 0;
}

int main(int argc, char **argv)
{
	if (argc > 1)
		dir = argv[1];
	setvbuf(stdout, NULL, _IOLBF, 0);
	printf("main b_two %d\n", b_two(1000));
	/* mov w0, #42; ret */
	uint32_t *w = (uint32_t *)dlsym(RTLD_DEFAULT, "b_two");
	char *page = (char *)((uintptr_t)w & ~(uintptr_t)4095);
	if (mprotect(page, 4096, PROT_READ | PROT_WRITE | PROT_EXEC)) { perror("mprotect"); return 1; }
	w[0] = 0x52800540;
	w[1] = 0xd65f03c0;
	__builtin___clear_cache((char *)w, (char *)(w + 2));
	mprotect(page, 4096, PROT_READ | PROT_EXEC);
	printf("main b_two rewritten %d\n", b_two(1000));
	pthread_t t;
	pthread_create(&t, 0, thread, 0);
	pthread_join(t, 0);
	return 0;
}
