/* libgex (wasm64, W64): running the signal frames gex-kernel leaves at a
 * syscall's return, for a program built LP64 and lowered to wasm32
 * (docs/wine-native-x64.md §1.1). The wasm32 form is ../libgex/signal.c.
 *
 * musl's wasm64 syscall stub calls this when the context's PC changed across
 * the syscall. The frame is x86-64's: RSP points at it (its first word is
 * the return address a handler would return to), RDI is the signal,
 * RSI = &info, RDX = &uc, and PC (slot 32) is the handler, a function-table
 * index. After the handler, rt_sigreturn restores the interrupted context
 * -- the syscall's result included -- and its PC, so the loop ends when PC
 * is back to what the stub saw. */
#include <signal.h>
#include <stdint.h>

__attribute__((__import_module__("gex"), __import_name__("syscall")))
void __gex_syscall_import(int, long long);

/* musl's gex_thread.s: make sp the C stack pointer, returning the old one */
unsigned long __gex_swap_sp(unsigned long);

#define X86_64_RT_SIGRETURN 15
enum { RAX = 0, RDX = 2, RSP = 4, RSI = 6, RDI = 7, PC = 32 };

typedef void (*rt_handler_t)(int, void *, void *);
typedef void (*handler_t)(int);

/* (its own frame, so this one stays small: it sits in those 256 bytes) */
static __attribute__((noinline)) int is_rt(int sig)
{
	struct sigaction sa;
	return sigaction(sig, 0, &sa) == 0 && (sa.sa_flags & SA_SIGINFO);
}

void __gex_signal_frames(volatile unsigned long long *x, unsigned long long pc)
{
	while (x[PC] != pc) {
		uintptr_t h = (uintptr_t)x[PC];
		unsigned long rsp = (unsigned long)x[RSP];
		int sig = (int)x[RDI];
		void *info = (void *)(uintptr_t)x[RSI], *uc = (void *)(uintptr_t)x[RDX];
		/* x86-64 frames are all rt frames, so which type the handler has
		 * (wasm traps on a mistyped call) is the action's SA_SIGINFO. A
		 * signal delivered at this query's own return is run first,
		 * nested, as on Linux. */
		/* below the frame and the x86-64 red zone, 16-aligned: the query
		 * too, as everything this calls, since the frame is only 256 bytes
		 * below the stack pointer the syscall stub had */
		unsigned long old = __gex_swap_sp((rsp - 128) & -16UL);
		int rt = is_rt(sig);
		if (rt)
			((rt_handler_t)h)(sig, info, uc);
		else
			((handler_t)h)(sig);
		__gex_swap_sp(old);
		/* as the restorer leaves it: the return address popped */
		x[RSP] = rsp + 8;
		x[RAX] = X86_64_RT_SIGRETURN;
		__gex_syscall_import((int)(unsigned long)x, 0);
	}
}
