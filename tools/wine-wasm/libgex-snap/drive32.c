/* The snapshot flavour's root for a wasm32 thread (snap.h): its driver.
 *
 * Each thread's record is mapped on its driver's first run and handed to
 * the host (gex.snap_register), which keeps it with the thread: the main
 * thread's driver runs before there is TLS. A thread the host says wants
 * none (gex.snap_wanted: a posix_spawn child, on its parent's memory) has
 * none, and a snapshot taken while it runs is refused. The unwind stops at
 * the driver, which hands it here: the record is filled in, the thread parks
 * in gex.snap_park, and the rewind makes the call again. A restored thread
 * starts in __gex_snap_resume on the same record. */
#include <stddef.h>
#include <sys/mman.h>
#include "snap.h"

SNAP_IMPORT("asyncify") int gex_asyncify(int, void *);
SNAP_IMPORT("snap_unwinding") void *gex_snap_unwinding(void);
SNAP_IMPORT("snap_park") void gex_snap_park(void *);

__attribute__((__visibility__("hidden"))) int __gex_fork_drive(int (*)(void *), void *, unsigned long);
int __gex_snap_resume(void *);

/* A restored thread's record, or 0 for any other thread. */
static struct snap *resumed(int (*fn)(void *), void *arg)
{
	return fn == __gex_snap_resume ? arg : 0;
}

/* A thread's driver starts: its record, mapped and registered (a restored
 * thread's is the one it resumes on), or 0. */
void *__gex_snap_begin(int (*fn)(void *), void *arg)
{
	struct snap *s = resumed(fn, arg);
	if (s || !gex_snap_wanted()) return s;
	s = mmap(0, SNAP_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (s == MAP_FAILED) return 0;
	__gex_snap_init(s);
	gex_snap_register(s, SNAP_DRIVER);
	return s;
}

/* Its driver returned: the thread is ending. */
void __gex_snap_end(void *p)
{
	if (!p) return;
	gex_snap_register(0, 0);
	munmap(p, SNAP_SIZE);
}

/* The driver stopped an unwind: the snapshot's (1), parked and rewound
 * here, or a fork's (0). */
int __gex_snap_unwound(int (*fn)(void *), void *arg, unsigned long sp)
{
	struct snap *s = gex_snap_unwinding();
	if (!s) return 0;
	s->drv.fn = fn;
	s->drv.arg = arg;
	s->drv.sp = sp;
	__gex_snap_keep(s);
	__gex_snap_start(s, __gex_snap_resume);
	gex_snap_park(s);
	gex_asyncify(3, s);
	return 1;
}

/* A restored thread's start: its record as the snapshot left it (unwound,
 * its driver's function noted), rewound under a driver of its own.
 * Entered as a clone child (on the record's start words) with the thread
 * pointer it had, so its TLS is the one the snapshot holds; its context's
 * registers as they were at the park. */
__attribute__((__visibility__("default"), __export_name__("__gex_snap_resume")))
int __gex_snap_resume(void *p)
{
	struct snap *s = p;
	gex_snap_register(s, SNAP_DRIVER);
	__gex_snap_back(s);
	gex_asyncify(3, s);
	return __gex_fork_drive(s->drv.fn, s->drv.arg, s->drv.sp);
}
