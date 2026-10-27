/* The snapshot flavour's root for a wasm64 program's thread (snap.h): the
 * host's entry into the program from the guest code the thread runs.
 *
 * Such a thread runs the guest in the host's call-outs, and the guest's
 * system and Unix calls come back in through an entry function of the
 * host's, which calls the program's handler (a function-table entry). A
 * snapshot stops only a thread whose frames below that handler are the
 * host's outermost call-out and that entry: the unwind ends at the entry,
 * which keeps the record (__gex_snap_fill), parks, and rewinds the handler
 * in place. A restored thread starts in __gex_snap_resume64: the context's
 * registers back, the host rewinds the handler under an entry of its own
 * (gex.snap_entry_rewind), and the thread goes on as its outermost call-out
 * loop does, at the context's PC.
 *
 * A thread's record is mapped before its first call that can block and
 * kept as its thread-specific data, unmapped at the thread's exit. */
#include <stddef.h>
#include <stdint.h>
#include <pthread.h>
#include <sys/mman.h>
#include "snap.h"

SNAP_IMPORT("snap_entry_rewind") void gex_snap_entry_rewind(void *);
SNAP_IMPORT("call_out") int gex_snap_call_out(int, int64_t, int64_t, int64_t);
SNAP_IMPORT("asyncify") int gex_snap_asyncify(int, int);
SNAP_IMPORT("snap_unwinding") int gex_snap_unwinding(void);
SNAP_IMPORT("snap_park") void gex_snap_park(int);

int __gex_snap_resume64(void *);
int __gex_snap_resume_run(void *);
int __gex_snap_run_loop(int (*)(void *), void *);

static pthread_key_t key;
/* 0 no key yet, 1 being made, 2 made, 3 none to be had */
static volatile int key_state;

static void gone(void *p)
{
	gex_snap_register(0, 0);
	munmap(p, SNAP_SIZE);
}

/* The key, made by the first call that wants it (no constructor runs in
 * these programs). Making it may wait on a lock, a call that comes back
 * here: that call, and any other thread's meanwhile, goes without. */
static int have_key(void)
{
	if (key_state == 2) return 1;
	if (!__sync_bool_compare_and_swap(&key_state, 0, 1)) return 0;
	key_state = pthread_key_create(&key, gone) == 0 ? 2 : 3;
	return key_state == 2;
}

static void ready_as(int root)
{
	if (!have_key() || pthread_getspecific(key) || !gex_snap_wanted()) return;
	struct snap *s = mmap(0, SNAP_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (s == MAP_FAILED) return;
	__gex_snap_init(s);
	pthread_setspecific(key, s);
	gex_snap_register(s, root);
}

/* The calling thread's record made, before the host stops it in a call
 * that can block: a thread whose record waits for its first such call. */
__attribute__((__visibility__("default"), __export_name__("__gex_snap_ready")))
void __gex_snap_ready(void)
{
	ready_as(SNAP_ENTRY);
}

/* A thread's function (none: the main thread's libc start, crt1.c) run
 * until it returns, as the libc's thread start runs it: the thread's other
 * root, for a thread that waits with none of
 * the guest's code below it (a Unix-side thread of its own). An unwind the
 * snapshot began in it ends here: the record kept, parked, and the function
 * called again under the rewind. */
int __gex_snap_libc_start(void *);
__attribute__((__noinline__)) int __gex_snap_run_loop(int (*fn)(void *), void *arg)
{
	for (;;) {
		int r = fn ? fn(arg) : __gex_snap_libc_start(arg);
		struct snap *s = (struct snap *)(unsigned long)gex_snap_unwinding();
		if (!s) return r;
		gex_snap_asyncify(2, 0);
		s->drv.fn = fn;
		s->drv.arg = arg;
		s->idx = 0;
		__gex_snap_keep(s);
		__gex_snap_start(s, __gex_snap_resume_run);
		gex_snap_park((int)(unsigned long)s);
		gex_snap_asyncify(3, (int)(unsigned long)s);
	}
}

/* The main thread's record (the libc's start, under __gex_snap_run_loop):
 * made once the libc is up, before the program's main. */
void __gex_snap_main(void)
{
	ready_as(SNAP_DRIVER | SNAP_ENTRY);
}

int __gex_snap_run(int (*fn)(void *), void *arg)
{
	if (fn != __gex_snap_resume64 && fn != __gex_snap_resume_run) ready_as(SNAP_DRIVER | SNAP_ENTRY);
	return __gex_snap_run_loop(fn, arg);
}

/* A restored thread stopped in its function (__gex_snap_run_loop):
 * rewound there. */
__attribute__((__visibility__("default"), __export_name__("__gex_snap_resume_run")))
int __gex_snap_resume_run(void *p)
{
	struct snap *s = p;
	gex_snap_register(s, SNAP_DRIVER | SNAP_ENTRY);
	__gex_snap_back(s);
	gex_snap_asyncify(3, (int)(unsigned long)s);
	return __gex_snap_run_loop(s->drv.fn, s->drv.arg);
}

/* The host's entry stopped the unwind: the record kept for a restored
 * thread's start. */
__attribute__((__visibility__("default"), __export_name__("__gex_snap_fill")))
void __gex_snap_fill(void *p)
{
	struct snap *s = p;
	__gex_snap_keep(s);
	__gex_snap_start(s, __gex_snap_resume64);
}

__attribute__((__visibility__("default"), __export_name__("__gex_snap_resume64")))
int __gex_snap_resume64(void *p)
{
	struct snap *s = p;
	gex_snap_register(s, SNAP_DRIVER | SNAP_ENTRY);
	__gex_snap_back(s);
	gex_snap_entry_rewind(s);
	int ctx = (int)__gex_get_ctx();
	for (;;) {
		volatile unsigned long long *x = (volatile unsigned long long *)(unsigned long)ctx;
		gex_snap_call_out(ctx, (int64_t)x[32], 0, 0);
	}
}
