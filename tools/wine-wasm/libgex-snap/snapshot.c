/* libgex, the snapshot flavour (build-sysroot.sh GEX_SNAPSHOT=1, with the
 * libc's GEX_SNAPSHOT hooks): threads stopped for a session snapshot and
 * started again from one.
 *
 * A call that can block goes through gex.syscall_b, the host's system call
 * under another name: wcc's asyncify instruments what can reach it, and the
 * host may unwind the thread's frames there instead of making the call (it
 * was not made: its number and arguments stay in the thread's context). The
 * unwind stops at the thread's driver (the libc's __gex_fork_drive), which
 * hands it here: the record is filled in, the thread parks in
 * gex.snap_park until it goes on in this session, and the rewind makes the
 * call again. A restored session's thread starts as a clone child in
 * __gex_snap_resume on the same record, in the memory the snapshot holds.
 *
 * Each thread's record is mapped on its driver's first run and handed to
 * the host (gex.snap_register), which keeps it with the thread: the main
 * thread's driver runs before there is TLS. A thread the host says wants
 * none (gex.snap_wanted: a posix_spawn child, on its parent's memory) has
 * none, and a snapshot taken while it runs is refused. */
#include <stddef.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/syscall.h>

#define IMPORT(name) __attribute__((__import_module__("gex"), __import_name__(name)))
IMPORT("syscall") void gex_syscall(int, long long);
IMPORT("syscall_b") void gex_syscall_b(int, long long);
IMPORT("asyncify") int gex_asyncify(int, void *);
IMPORT("snap_wanted") int gex_snap_wanted(void);
IMPORT("snap_register") void gex_snap_register(void *);
IMPORT("snap_unwinding") void *gex_snap_unwinding(void);
IMPORT("snap_park") void gex_snap_park(void *);

unsigned long __gex_get_ctx(void);
unsigned long __gex_get_tp(void);
__attribute__((__visibility__("hidden"))) int __gex_fork_drive(int (*)(void *), void *, unsigned long);
int __gex_snap_resume(void *);

struct driver {
	int (*fn)(void *);
	void *arg;
	unsigned long sp;
};

/* A record: asyncify's data first ({cur, end}, as the libc fork's). At the
 * park the driver's function is noted, and the context's registers (the
 * parked call's number and arguments, its PC), which a restored thread's
 * start overwrites and __gex_snap_resume puts back; and the start a
 * restored thread is given: a clone child's words (the libc's wasm32
 * __clone) at the record's top, on a stack below them that the start runs
 * on until the rewind is under way. `entry` and `start` are what the host
 * gives the restored thread's context (its PC, the image's thread entry,
 * and its stack pointer); `sp_at` the host's, the program's stack pointer
 * at the call. The host reads and writes the words at offsets 0 to 20. */
struct snap {
	char *cur, *end;
	unsigned long entry, start;
	unsigned long sp_at, spare;
	unsigned long long regs[8], pc;
	struct driver drv;
};

#define SNAP_SIZE (256 << 10)
#define SNAP_STACK 16384

/* The calls that can wait: a snapshot stops a thread only in one of these. */
static int blocks(long n)
{
	switch (n) {
#define B(x) case x:
#ifdef SYS_read
	B(SYS_read)
#endif
#ifdef SYS_readv
	B(SYS_readv)
#endif
#ifdef SYS_write
	B(SYS_write)
#endif
#ifdef SYS_writev
	B(SYS_writev)
#endif
#ifdef SYS_poll
	B(SYS_poll)
#endif
#ifdef SYS_ppoll
	B(SYS_ppoll)
#endif
#ifdef SYS_ppoll_time64
	B(SYS_ppoll_time64)
#endif
#ifdef SYS__newselect
	B(SYS__newselect)
#endif
#ifdef SYS_select
	B(SYS_select)
#endif
#ifdef SYS_pselect6
	B(SYS_pselect6)
#endif
#ifdef SYS_pselect6_time64
	B(SYS_pselect6_time64)
#endif
#ifdef SYS_epoll_wait
	B(SYS_epoll_wait)
#endif
#ifdef SYS_epoll_pwait
	B(SYS_epoll_pwait)
#endif
#ifdef SYS_epoll_pwait2
	B(SYS_epoll_pwait2)
#endif
#ifdef SYS_futex
	B(SYS_futex)
#endif
#ifdef SYS_futex_time64
	B(SYS_futex_time64)
#endif
#ifdef SYS_nanosleep
	B(SYS_nanosleep)
#endif
#ifdef SYS_clock_nanosleep
	B(SYS_clock_nanosleep)
#endif
#ifdef SYS_clock_nanosleep_time64
	B(SYS_clock_nanosleep_time64)
#endif
#ifdef SYS_wait4
	B(SYS_wait4)
#endif
#ifdef SYS_waitid
	B(SYS_waitid)
#endif
#ifdef SYS_waitpid
	B(SYS_waitpid)
#endif
#ifdef SYS_accept
	B(SYS_accept)
#endif
#ifdef SYS_accept4
	B(SYS_accept4)
#endif
#ifdef SYS_connect
	B(SYS_connect)
#endif
#ifdef SYS_recvfrom
	B(SYS_recvfrom)
#endif
#ifdef SYS_recvmsg
	B(SYS_recvmsg)
#endif
#ifdef SYS_recvmmsg
	B(SYS_recvmmsg)
#endif
#ifdef SYS_recvmmsg_time64
	B(SYS_recvmmsg_time64)
#endif
#ifdef SYS_sendto
	B(SYS_sendto)
#endif
#ifdef SYS_sendmsg
	B(SYS_sendmsg)
#endif
#ifdef SYS_sendmmsg
	B(SYS_sendmmsg)
#endif
#ifdef SYS_socketcall
	B(SYS_socketcall)
#endif
#ifdef SYS_rt_sigsuspend
	B(SYS_rt_sigsuspend)
#endif
#ifdef SYS_rt_sigtimedwait
	B(SYS_rt_sigtimedwait)
#endif
#ifdef SYS_rt_sigtimedwait_time64
	B(SYS_rt_sigtimedwait_time64)
#endif
#ifdef SYS_pause
	B(SYS_pause)
#endif
#ifdef SYS_flock
	B(SYS_flock)
#endif
#ifdef SYS_fcntl
	B(SYS_fcntl)
#endif
#ifdef SYS_fcntl64
	B(SYS_fcntl64)
#endif
#ifdef SYS_mq_timedreceive
	B(SYS_mq_timedreceive)
#endif
#ifdef SYS_mq_timedreceive_time64
	B(SYS_mq_timedreceive_time64)
#endif
#ifdef SYS_msgrcv
	B(SYS_msgrcv)
#endif
#undef B
		return 1;
	}
	return 0;
}

/* The libc's system call (its wasm32 __gex_sc), the context filled in. */
void __gex_snap_syscall(int x)
{
	if (blocks((long)((volatile unsigned long long *)x)[0])) gex_syscall_b(x, 0);
	else gex_syscall(x, 0);
}

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
	s->cur = (char *)(s + 1);
	s->end = (char *)s + SNAP_SIZE - SNAP_STACK;
	gex_snap_register(s);
	return s;
}

/* Its driver returned: the thread is ending. */
void __gex_snap_end(void *p)
{
	if (!p) return;
	gex_snap_register(0);
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
	volatile unsigned long long *x = (volatile unsigned long long *)__gex_get_ctx();
	for (int i = 0; i < 8; i++) s->regs[i] = x[i];
	s->pc = x[32];
	unsigned long *start = (unsigned long *)((char *)s + SNAP_SIZE) - 4;
	start[0] = (unsigned long)__gex_snap_resume;
	start[1] = (unsigned long)s;
	start[2] = __gex_get_tp();
	start[3] = 0;
	s->entry = getauxval(AT_ENTRY) + 16;
	s->start = (unsigned long)start;
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
	gex_snap_register(s);
	volatile unsigned long long *x = (volatile unsigned long long *)__gex_get_ctx();
	for (int i = 0; i < 8; i++) x[i] = s->regs[i];
	x[32] = s->pc;
	gex_asyncify(3, s);
	return __gex_fork_drive(s->drv.fn, s->drv.arg, s->drv.sp);
}
