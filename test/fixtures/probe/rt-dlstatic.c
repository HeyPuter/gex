/* dlopen in a static program, for the runtime's core/dlopen-static:
 *
 *   rt-dlstatic SO   dlopens SO (rt-side.c, a side module whose imports this
 *                    program exports: images.mjs links it -Wl,--export=
 *                    each), prints "dlopen MS ms", calls into it from two
 *                    threads, each call calling back into this program's
 *                    libc; then "ok", or "FAIL WHAT" for each check that
 *                    failed */
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static char *(*dup)(const char *, int);
static double (*math)(double);
static int bad;

static void check(const char *what, int ok) {
  if (!ok) {
    printf("FAIL %s\n", what);
    __atomic_store_n(&bad, 1, __ATOMIC_SEQ_CST);
  }
}

static void *th(void *a) {
  int k = (int)(long)a;
  for (int i = 0; i < 1000; i++) {
    char want[32], *s = dup("t", k * 1000 + i);
    snprintf(want, sizeof want, "t-%d", k * 1000 + i);
    check("a thread's call", strcmp(s, want) == 0);
    free(s);
  }
  double m = math(0.5);
  check("a thread's call through the side module's pointer to cos", m > 0.9999 && m < 1.0001);
  return 0;
}

int main(int argc, char **argv) {
  if (argc < 2) return 2;
  struct timespec t0, t1;
  clock_gettime(CLOCK_MONOTONIC, &t0);
  void *h = dlopen(argv[1], RTLD_NOW);
  clock_gettime(CLOCK_MONOTONIC, &t1);
  if (!h) {
    printf("FAIL dlopen: %s\n", dlerror());
    return 1;
  }
  printf("dlopen %.1f ms\n", (t1.tv_sec - t0.tv_sec) * 1e3 + (t1.tv_nsec - t0.tv_nsec) / 1e6);
  dup = (char *(*)(const char *, int))dlsym(h, "side_dup");
  math = (double (*)(double))dlsym(h, "side_math");
  int (*ctor)(void) = (int (*)(void))dlsym(h, "side_ctor");
  int *calls = (int *)dlsym(h, "side_calls");
  void (*log)(const char *) = (void (*)(const char *))dlsym(h, "side_log");
  if (!dup || !math || !ctor || !calls || !log) {
    printf("FAIL dlsym\n");
    return 1;
  }
  check("the constructor ran once", ctor() == 1);
  log("hello");
  pthread_t t[2];
  for (int k = 0; k < 2; k++) pthread_create(&t[k], 0, th, (void *)(long)(k + 1));
  for (int k = 0; k < 2; k++) pthread_join(t[k], 0);
  check("the calls counted", *calls == 2000);
  check("dlopen again gives the same handle", dlopen(argv[1], RTLD_NOW) == h);
  check("the constructor ran once after the second dlopen", ctor() == 1);
  printf("%s\n", bad ? "FAIL" : "ok");
  return bad;
}
