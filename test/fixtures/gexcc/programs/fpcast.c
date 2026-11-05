/* A call through a function pointer of another type (old C: callbacks
 * declared as int (*)() and called with arguments, cleanups returning int
 * stored as void (*)(void *)). wasm traps on it; with -fgex-fpcast-emu
 * (GEXCC_FPCAST=1) gexcc links it with binaryen's cast emulation and it
 * behaves as on x86. run.sh builds files whose name starts with fpcast this way. */
#include <stdio.h>

static int counted;
static int returns_int(void *p) { counted += *(int *)p; return 42; }
static void takes_two(int a, int b) { counted += a + b; }

typedef void (*cleanup_t)(void *);
typedef void (*thunk_t)(int);

int main(void)
{
	int v = 5;
	cleanup_t c = (cleanup_t)returns_int;
	c(&v);
	thunk_t t = (thunk_t)takes_two;
	counted = 5;
	t(3);
	printf("survived, first call counted %d\n", counted != 0);
	return 0;
}
