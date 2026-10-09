/* The runtime of an item-3 library on the LP64 libc (sysroot.sh: musl's
 * wasm64a64 arch) in place of rt.c: the adapters every replaced library
 * has, and what musl asks of a provider (its threads' entry and stack,
 * signal handlers at a syscall's return). malloc is musl's, over mmap'd
 * guest pages (brk stays the guest program's). */
#include "item3.h"

__attribute__((import_module("gex"), import_name("provider_missing"))) void gexp_host_missing(uint64_t addr);
__attribute__((import_module("gex"), import_name("provider_thread_entry"))) uint64_t gexp_host_thread_entry(void);
__attribute__((import_module("gex"), import_name("provider_stack"))) uint64_t gexp_host_stack(void);
__attribute__((import_module("gex"), import_name("syscall"))) void gexp_host_syscall(int ctx, long long pc);

GEXP(gexp__missing) { gexp_host_missing(A64_X(ctx, LAST_TARGET)); __builtin_trap(); }
/* Bound at the replaced library's constructors and destructors. */
GEXP(gexp__noop) { return a64_ret(ctx, ret, A64_X(ctx, 0)); }

/* musl's clone starts a thread the library makes at the address the host
 * bound to __gex_thread_start (musl's), on the C stack the host gives
 * each thread (shims libprov). */
unsigned long __gex_thread_entry(void) { return gexp_host_thread_entry(); }
int __gex_thread_stack(unsigned long *lo, unsigned long *size)
{
	*lo = gexp_host_stack();
	*size = 1ul << 20;
	return 0;
}

/* A signal handled at one of the library's syscalls: gex-kernel's aarch64
 * personality has left a signal frame on the guest stack (SP at it), the
 * handler's arguments in x0-x2 and PC at the handler. Run it as a guest
 * call, then do what its return to the restorer would (rt_sigreturn),
 * which puts back the interrupted context: the syscall's result, and PC
 * where the stub was. */
#define A64_RT_SIGRETURN 139
void __gex_signal_frames(volatile unsigned long long *x, unsigned long long pc)
{
	while (x[A64_PC] != pc) {
		uint64_t a[3] = { x[0], x[1], x[2] };
		a64_call_guest(x[A64_PC], 3, a);
		x[8] = A64_RT_SIGRETURN;
		gexp_host_syscall((int)(uintptr_t)x, 0);
	}
}

/* musl's heap: zlib's and zstd's own frees reach it only with what their
 * own allocators made (a stream keeps the allocator pair it started with). */
int gexp_heap_owns(const void *p) { (void)p; return 1; }
