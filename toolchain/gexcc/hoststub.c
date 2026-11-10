/* The host launcher in a gexcc program's container (wasm-elf.py --host-stub).
 *
 * The container is an ELF32 i386 executable. gex never runs its x86 code: it
 * binds the wasm module at the entry. A Linux x86 host with IA-32 emulation
 * does run it, and lands here: we re-exec the same file under gexc, so a
 * cross-built program (a configure test, a `make check` binary) runs on the
 * build machine like a native one.
 *
 *   runner: $GEXCC_RUNNER, else the path baked into the container at link
 *           time (the trailer below: GEXCC_BAKE_RUNNER), else `gexcc-exec`
 *           on $PATH (gexcc's install: its slots bound how many run at
 *           once), else `gexc` on $PATH
 *   argv:   RUNNER run --host-env --argv0 ARGV0 /proc/self/exe's target ARGS...
 *
 * Freestanding, position-independent i386 code; nothing but syscalls.
 * Built by build.sh into hoststub.bin. */

#define SYS_exit 1
#define SYS_write 4
#define SYS_execve 11
#define SYS_readlink 85

static long sys3(long n, long a, long b, long c)
{
	long r;
	__asm__ volatile("int $0x80" : "=a"(r) : "a"(n), "b"(a), "c"(b), "d"(c) : "memory");
	return r;
}

/* Filled by wasm-elf.py: the runner's path, NUL-terminated (empty: none). */
__attribute__((section(".text.zcfg"), used, aligned(4))) char gexcc_runner[256] = "GEXCC-RUNNER";

static int starts(const char *s, const char *p)
{
	while (*p)
		if (*s++ != *p++)
			return 0;
	return 1;
}

static unsigned slen(const char *s)
{
	unsigned n = 0;
	while (s[n])
		n++;
	return n;
}

static void say(const char *s)
{
	sys3(SYS_write, 2, (long)s, slen(s));
}

__attribute__((used)) void gexcc_host_main(long *sp)
{
	long argc = sp[0];
	char **argv = (char **)(sp + 1);
	char **envp = argv + argc + 1;
	char self[4096];
	long n = sys3(SYS_readlink, (long)"/proc/self/exe", (long)self, sizeof self - 1);
	if (n <= 0) {
		say("gexcc: cannot read /proc/self/exe\n");
		sys3(SYS_exit, 127, 0, 0);
	}
	self[n] = 0;
	const char *runner = 0, *path = "/usr/local/bin:/usr/bin:/bin";
	for (char **e = envp; *e; e++) {
		if (starts(*e, "GEXCC_RUNNER=") && (*e)[13])
			runner = *e + 13;
		else if (starts(*e, "PATH="))
			path = *e + 5;
	}
	if (!runner && gexcc_runner[0] && !starts(gexcc_runner, "GEXCC-RUNNER"))
		runner = gexcc_runner;
	char *nargv[argc + 8];
	int k = 0;
	nargv[k++] = (char *)"gexc";
	nargv[k++] = (char *)"run";
	nargv[k++] = (char *)"--host-env";
	nargv[k++] = (char *)"--argv0";
	nargv[k++] = argc > 0 ? argv[0] : self;
	nargv[k++] = self;
	for (long i = 1; i < argc; i++)
		nargv[k++] = argv[i];
	nargv[k] = 0;
	if (runner) {
		nargv[0] = (char *)runner;
		sys3(SYS_execve, (long)runner, (long)nargv, (long)envp);
		say("gexcc: cannot run the gexc named by GEXCC_RUNNER or the container\n");
		sys3(SYS_exit, 127, 0, 0);
	}
	/* `gexcc-exec`, then `gexc`, on $PATH */
	char buf[4096];
	for (int w = 0; w < 2; w++) {
		/* (no table of pointers: nothing relocates this code's data) */
		const char *nm = w ? "gexc" : "gexcc-exec";
		unsigned nl = 0;
		while (nm[nl])
			nl++;
		const char *p = path;
		for (;;) {
			const char *q = p;
			while (*q && *q != ':')
				q++;
			unsigned l = q - p;
			if (l && l + nl + 2 < sizeof buf) {
				for (unsigned i = 0; i < l; i++)
					buf[i] = p[i];
				buf[l] = '/';
				for (unsigned i = 0; i <= nl; i++)
					buf[l + 1 + i] = nm[i];
				nargv[0] = buf;
				sys3(SYS_execve, (long)buf, (long)nargv, (long)envp);
			}
			if (!*q)
				break;
			p = q + 1;
		}
	}
	say("gexcc: this is a gex wasm32 program and no gexc was found (set GEXCC_RUNNER or put gexcc-exec or gexc on PATH)\n");
	sys3(SYS_exit, 127, 0, 0);
}

__asm__(".section .text.start,\"ax\"\n"
	".globl _start\n"
	"_start:\n"
	"  mov %esp, %eax\n"
	"  and $-16, %esp\n"
	"  sub $12, %esp\n"
	"  push %eax\n"
	"  call gexcc_host_main\n"
	"  hlt\n");
