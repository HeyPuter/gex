/* libgex: dlopen, dlsym and dlclose over a registry linked into the
 * program, and gex's loader for wasm32 shared libraries.
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

/* gex's loader (shims' dylink): a wasm32 shared library loaded into the
 * process, for a program linked against shared libraries (docs/gexcc.md
 * 7.6). Handles from it are GEX_DL_HANDLE and above; 0 with a message in
 * err when it cannot. A static program's loader has no link map and says
 * so. */
/* a pointer as the address gex's imports take: 32-bit on wasm32 and on
 * wasm64 lowered to wasm32 (every address below 4 GiB); 64-bit in a
 * memory64 flavour (GEX_MEMORY64, docs/wine-native-x64.md §0.1) */
#ifdef GEX_MEMORY64
typedef unsigned long long gex_addr_t;
#define GEX_ADDR(p) ((gex_addr_t)(unsigned long)(p))
#else
typedef unsigned gex_addr_t;
#define GEX_ADDR(p) ((gex_addr_t)(unsigned long)(p))
#endif
__attribute__((__import_module__("gex"), __import_name__("dlopen")))
unsigned __gex_dlopen(gex_addr_t path, gex_addr_t err, unsigned errlen);
__attribute__((__import_module__("gex"), __import_name__("dlsym")))
unsigned __gex_dlsym(unsigned handle, gex_addr_t name);
#define GEX_DL_HANDLE 0xfff00000u
#define IS_GEX(p) ((unsigned long)(p) >= GEX_DL_HANDLE)

void *dlopen(const char *file, int mode)
{
	if (!file) return (void *)&self;
	const char *b = base(file);
	for (const struct __gex_dl_lib *const *l = __gex_dl_libs; *l; l++)
		if (!strcmp((*l)->name, b)) return (void *)*l;
	char err[256];
	err[0] = 0;
	unsigned h = __gex_dlopen(GEX_ADDR(file), GEX_ADDR(err), sizeof err);
	if (h) return (void *)(unsigned long)h;
	__dl_seterr("%s", err[0] ? err : "not linked into this program");
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
	if (IS_GEX(p)) {
		void *a = (void *)(unsigned long)__gex_dlsym(GEX_ADDR(p), GEX_ADDR(s));
		if (a) return a;
	} else if (p && p != RTLD_DEFAULT && p != (void *)&self) {
		void *a = find(p, s);
		if (a) return a;
	} else {
		for (const struct __gex_dl_lib *const *l = __gex_dl_libs; *l; l++) {
			void *a = find(*l, s);
			if (a) return a;
		}
		/* and the process's global scope (shared libraries, and the
		 * program's own symbols when it is linked -rdynamic) */
		void *a = (void *)(unsigned long)__gex_dlsym(0, GEX_ADDR(s));
		if (a) return a;
	}
	__dl_seterr("Symbol not found: %s", s);
	return 0;
}
