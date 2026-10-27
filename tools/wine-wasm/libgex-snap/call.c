/* The snapshot flavour's system call (snap.h), and what both roots share. */
#include <stddef.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include "snap.h"

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

/* Whether the call numbered `n` is one a snapshot may stop a thread in:
 * the host asks, at a system call while a snapshot is pending (the libc's
 * calls are the default build's, through gex.syscall). */
__attribute__((__visibility__("default"), __export_name__("__gex_snap_blocks")))
int __gex_snap_blocks(long n)
{
	return blocks(n);
}

void __gex_snap_init(struct snap *s)
{
	s->cur = (uint32_t)(unsigned long)(s + 1);
	s->end = (uint32_t)((unsigned long)s + SNAP_SIZE - SNAP_STACK);
}

void __gex_snap_start(struct snap *s, int (*resume)(void *))
{
	unsigned long *start = (unsigned long *)((char *)s + SNAP_SIZE) - 4;
	start[0] = (unsigned long)resume;
	start[1] = (unsigned long)s;
	start[2] = __gex_get_tp();
	start[3] = 0;
	s->entry = (uint32_t)(getauxval(AT_ENTRY) + 16);
	s->start = (uint32_t)(unsigned long)start;
}

void __gex_snap_keep(struct snap *s)
{
	volatile unsigned long long *x = (volatile unsigned long long *)__gex_get_ctx();
	for (int i = 0; i < 16; i++) s->regs[i] = x[i];
	s->pc = x[32];
}

void __gex_snap_back(struct snap *s)
{
	volatile unsigned long long *x = (volatile unsigned long long *)__gex_get_ctx();
	for (int i = 0; i < 16; i++) x[i] = s->regs[i];
	x[32] = s->pc;
}
