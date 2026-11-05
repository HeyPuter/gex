/* Function-tier units a thread's Worker instantiates at their first call
   (hosts/browser/js/gex.mjs `#cgenDefer`). The main thread calls libb.so's
   b_two through libe.so's unit (the probe cache now names b_two's index),
   then maps new code over b_two's page (the unit's function retired; with
   GEX_TEST_KEEP_PROBE its probe entry stays). A thread then dlopens
   libd.so, whose constructor calls libb.so's b_mul (libb.so's unit
   instantiated in the thread's Worker from inside that call, its element
   segment filling b_two's retired slot too), and calls b_two through
   libe.so: the stale probe entry reaches that slot without `lookup`, and
   must find it retired, so the new code runs. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>

int b_mul(int);
int e_two(void);

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
	printf("thread e_two %d\n", e_two());
	printf("thread b_mul %d\n", b_mul(6));
	return 0;
}

int main(int argc, char **argv)
{
	if (argc > 1)
		dir = argv[1];
	setvbuf(stdout, NULL, _IOLBF, 0);
	printf("main e_two %d\n", e_two());
	/* b_two's page replaced: mov w0, #42; ret */
	uint32_t *w = (uint32_t *)dlsym(RTLD_DEFAULT, "b_two");
	void *page = (void *)((uintptr_t)w & ~(uintptr_t)4095);
	if (munmap(page, 4096)) { perror("munmap"); return 1; }
	if (mmap(page, 4096, PROT_READ | PROT_WRITE | PROT_EXEC, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) != page) { perror("mmap"); return 1; }
	w[0] = 0x52800540;
	w[1] = 0xd65f03c0;
	__builtin___clear_cache((char *)w, (char *)(w + 2));
	pthread_t t;
	pthread_create(&t, 0, thread, 0);
	pthread_join(t, 0);
	return 0;
}
