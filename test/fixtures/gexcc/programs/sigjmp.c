/* sigsetjmp/siglongjmp (bash's error recovery): out of nested calls and
 * out of a signal handler, with the mask restored when asked; _setjmp. */
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>

static sigjmp_buf top;
static jmp_buf plain;

static void deep(int n)
{
	if (n == 0)
		siglongjmp(top, 7);
	deep(n - 1);
}

static void on_usr1(int s)
{
	(void)s;
	siglongjmp(top, 9);
}

int main(void)
{
	volatile int round = 0;
	sigset_t cur;
	int r = sigsetjmp(top, 1);
	printf("sigsetjmp -> %d\n", r);
	if (round == 0) {
		round = 1;
		deep(5);
	}
	if (round == 1) {
		round = 2;
		signal(SIGUSR1, on_usr1);
		kill(getpid(), SIGUSR1);
		printf("not reached\n");
	}
	sigprocmask(SIG_BLOCK, 0, &cur);
	printf("SIGUSR1 blocked after the handler's jump: %d\n", sigismember(&cur, SIGUSR1));
	if (_setjmp(plain) == 0)
		_longjmp(plain, 3);
	printf("_setjmp done\n");
	return 0;
}
