/* The side module rt-dlstatic loads (core/dlopen-static): its imports, libc's
 * among them, are the program's exports. Data (a counter under a mutex, a
 * pointer to the program's cos), a constructor, and calls back into the
 * program's malloc, snprintf, sin and fprintf. */
#include <math.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int side_calls;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int ctor_ran;
double (*side_fn)(double) = cos;

__attribute__((constructor)) static void init(void) { ctor_ran++; }

int side_ctor(void) { return ctor_ran; }

char *side_dup(const char *s, int k) {
  size_t n = strlen(s) + 16;
  char *p = malloc(n);
  snprintf(p, n, "%s-%d", s, k);
  pthread_mutex_lock(&lock);
  side_calls++;
  pthread_mutex_unlock(&lock);
  return p;
}

double side_math(double x) { return side_fn(x) * side_fn(x) + sin(x) * sin(x); }

void side_log(const char *s) { fprintf(stderr, "side: %s\n", s); }
