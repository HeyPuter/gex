/* libgex: running the signal frames gex-kernel's wasm32 personality leaves
 * at a syscall's return (docs/wine-native.md §3.11.2, "Signals").
 *
 * musl's wasm32 syscall stub (arch/wasm32/syscall_arch.h) calls this when
 * the context's PC changed across the syscall. The frame is i386's: ESP
 * points at it, EAX is the signal, and an rt frame (SA_SIGINFO) has
 * EDX = &info, ECX = &uc. After the handler, sigreturn restores the
 * interrupted context -- the syscall's result included -- and its PC, so
 * the loop ends when PC is back to what the stub saw.
 *
 * Kept out of musl: the frame layout is gex-kernel's protocol, not the
 * libc's; musl's stub only names this function. */
#include <stdint.h>

__attribute__((__import_module__("gex"), __import_name__("syscall")))
void __gex_syscall_import(int, long long);

/* signal_call.s: the handler on the C stack at sp */
void __gex_call_handler(unsigned long, int, unsigned long, unsigned long, unsigned long, int);

#define I386_SIGRETURN 119
#define I386_RT_SIGRETURN 173

void __gex_signal_frames(volatile unsigned long long *x, unsigned long long pc)
{
	while (x[32] != pc) {
		unsigned long handler = (unsigned long)x[32];
		unsigned long esp = (unsigned long)x[4];
		int sig = (int)x[0];
		unsigned long uc = (unsigned long)x[1], info = (unsigned long)x[2];
		int rt = uc != 0;
		__gex_call_handler(handler, sig, info, uc, (esp - 16) & -16UL, rt);
		/* As the i386 restorer leaves it: the return address popped, and
		 * for the old frame the signal number too. */
		x[4] = esp + (rt ? 4 : 8);
		x[0] = rt ? I386_RT_SIGRETURN : I386_SIGRETURN;
		__gex_syscall_import((int)(unsigned long)x, 0);
	}
}
