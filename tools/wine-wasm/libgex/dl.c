/* libgex: dlopen, dlsym and dlclose over a registry linked into the
 * program.
 *
 * gex runs a closed world (docs/wine-native.md §3.12): every library a
 * program will dlopen -- Wine's Unix libraries -- is linked into it at
 * build time, each with its exported symbols renamed apart, and the link
 * adds a registry (tools/wine-wasm/dlreg.py) naming each library and its
 * symbols. dlopen finds a library by its file name (any directory), dlsym
 * finds a symbol in it, or in every library for RTLD_DEFAULT. Nothing is
 * loaded at run time, so dlclose does nothing.
 *
 * Kept out of musl (§3.11.2): a static musl's dlopen and __dlsym are weak
 * stubs, and wcc links this library before libc, so these replace them.
 * dlclose is defined here, next to dlopen, so that libc's (which reports
 * every handle invalid in a static libc) is never pulled in. Errors go
 * through musl's __dl_seterr, which dlerror reports. */
#include <dlfcn.h>
#include <string.h>

__attribute__((__visibility__("hidden")))
void __dl_seterr(const char *, ...);

struct __gex_dl_sym { const char *name; void *addr; };
struct __gex_dl_lib { const char *name; const struct __gex_dl_sym *syms; };

/* The program's registry, NULL-terminated; empty unless one is linked. */
static const struct __gex_dl_lib *const empty[] = { 0 };
extern __typeof(empty) __gex_dl_libs __attribute__((__weak__, __alias__("empty")));

/* The handle dlopen(NULL) gives: every library. */
static const struct __gex_dl_lib self = { "", 0 };

static const char *base(const char *p)
{
	const char *s = strrchr(p, '/');
	return s ? s + 1 : p;
}

void *dlopen(const char *file, int mode)
{
	if (!file) return (void *)&self;
	const char *b = base(file);
	for (const struct __gex_dl_lib *const *l = __gex_dl_libs; *l; l++)
		if (!strcmp((*l)->name, b)) return (void *)*l;
	__dl_seterr("%s: not linked into this program", file);
	return 0;
}

int dlclose(void *p)
{
	return 0;
}

static void *find(const struct __gex_dl_lib *l, const char *s)
{
	for (const struct __gex_dl_sym *y = l->syms; y && y->name; y++)
		if (!strcmp(y->name, s)) return y->addr;
	return 0;
}

/* musl's dlsym calls __dlsym (hidden: a static link resolves it). */
__attribute__((__visibility__("hidden")))
void *__dlsym(void *restrict p, const char *restrict s, void *restrict ra)
{
	if (p && p != RTLD_DEFAULT && p != (void *)&self) {
		void *a = find(p, s);
		if (a) return a;
	} else {
		for (const struct __gex_dl_lib *const *l = __gex_dl_libs; *l; l++) {
			void *a = find(*l, s);
			if (a) return a;
		}
	}
	__dl_seterr("Symbol not found: %s", s);
	return 0;
}
