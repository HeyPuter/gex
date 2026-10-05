/* Loads libgt.so (argv[1]) with dlopen and calls it from three threads; see
 * libgt.c. Run natively and under gex with GEX_WASM_DLLS=libgt.so=gt.wasm. */
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

static int (*gt_add)(int, int);
static long (*gt_sum8)(long, long, long, long, long, long, long, long);
static double (*gt_scale)(double, int, float);
static const char *(*gt_greet)(const char *);
static long (*gt_counter)(void);
static long (*gt_heap)(int);
static int (*gt_write)(const char *);
static int (*gt_getenv)(const char *, char *, int);
static long (*gt_threads)(int);
static const char *(*gt_impl)(void);
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static void *each(void *arg)
{
	long id = (long)arg;
	long c = 0;
	for (int i = 0; i < 1000 + 100 * id; i++) c = gt_counter();
	long h = gt_heap(500 + (int)id);
	char name[16];
	snprintf(name, sizeof name, "thread%ld", id);
	const char *g = gt_greet(name);
	pthread_mutex_lock(&lock);
	printf("[%ld] counter %ld heap %ld greet '%s'\n", id, c, h, g);
	pthread_mutex_unlock(&lock);
	return 0;
}

#define SYM(n) do { *(void **)&n = dlsym(h, #n); if (!n) { printf("no %s\n", #n); return 1; } } while (0)

int main(int argc, char **argv)
{
	void *h = dlopen(argc > 1 ? argv[1] : "./libgt.so", RTLD_NOW);
	if (!h) { printf("dlopen: %s\n", dlerror()); return 1; }
	SYM(gt_add); SYM(gt_sum8); SYM(gt_scale); SYM(gt_greet); SYM(gt_counter);
	SYM(gt_heap); SYM(gt_write); SYM(gt_getenv); SYM(gt_threads); SYM(gt_impl);
	printf("impl %s\n", gt_impl());
	printf("add %d\n", gt_add(40, 2));
	printf("sum8 %ld\n", gt_sum8(1, 2, 3, 4, 5, 6, 7, 8));
	printf("scale %.3f\n", gt_scale(1.5, 3, 0.25f));
	printf("greet '%s'\n", gt_greet("main"));
	char buf[64];
	printf("getenv %d '%s'\n", gt_getenv("GT_VALUE", buf, sizeof buf), buf);
	fflush(stdout);
	gt_write("write from the library\n");
	pthread_t t[2];
	for (long i = 0; i < 2; i++) pthread_create(&t[i], 0, each, (void *)(i + 1));
	for (int i = 0; i < 2; i++) pthread_join(t[i], 0);
	each(0);
	printf("threads %ld\n", gt_threads(4));
	return 0;
}
