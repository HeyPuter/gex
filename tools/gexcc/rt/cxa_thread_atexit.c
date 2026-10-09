// thread_local destructors. libc++abi's __cxa_thread_atexit calls
// __cxa_thread_atexit_impl when the C library has one (glibc) through a weak
// reference; musl has none, and in a -pie wasm32 link the weak reference
// becomes a GOT.func import that nothing provides (apt's programs failed to
// instantiate). gexc++ links this one in (-u): a per-thread list, run in
// reverse order at thread exit (a pthread key's destructor) and, for the
// thread that calls exit(), from an atexit handler.
#include <pthread.h>
#include <stdlib.h>

struct dtor {
  void (*fn)(void *);
  void *obj;
  struct dtor *next;
};

static pthread_key_t key;
static pthread_once_t once = PTHREAD_ONCE_INIT;

static void run(struct dtor *d) {
  while (d) {
    struct dtor *n = d->next;
    d->fn(d->obj);
    free(d);
    d = n;
  }
}

// a destructor may register more: each pass takes the list as it stands
static void drain(void) {
  struct dtor *d;
  while ((d = pthread_getspecific(key))) {
    pthread_setspecific(key, 0);
    run(d);
  }
}

static void at_thread_exit(void *p) {
  run(p);
  drain();
}

static void at_exit(void) { drain(); }

static void init(void) {
  pthread_key_create(&key, at_thread_exit);
  atexit(at_exit);
}

int __cxa_thread_atexit_impl(void (*fn)(void *), void *obj, void *dso) {
  (void)dso;
  struct dtor *d = malloc(sizeof *d);
  if (!d) return -1;
  pthread_once(&once, init);
  d->fn = fn;
  d->obj = obj;
  d->next = pthread_getspecific(key);
  pthread_setspecific(key, d);
  return 0;
}
