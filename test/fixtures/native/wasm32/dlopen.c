/* dlopen, dlsym, dlerror on gex's wasm32 (libgex's registry): a library named in the program's registry is found by its file
 * name in any directory, its symbols by name; anything else fails with a
 * dlerror message, as it does natively. The registry is gex's closed world,
 * so the native build calls the function directly and prints the same. */
#include <dlfcn.h>
#include <stdio.h>

static int twice(int x) { return 2 * x; }
int twice_data = 21;

#ifdef __wasm__
struct __gex_dl_sym { const char *name; void *addr; };
struct __gex_dl_lib { const char *name; const struct __gex_dl_sym *syms; };
static const struct __gex_dl_sym twice_syms[] = {
	{ "twice", (void *)twice }, { "twice_data", &twice_data }, { 0, 0 } };
static const struct __gex_dl_lib twice_lib = { "libtwice.so", twice_syms };
const struct __gex_dl_lib *const __gex_dl_libs[] = { &twice_lib, 0 };
#endif

int main(void)
{
	void *h = dlopen("/no/such/dir/libnope.so", RTLD_NOW);
	printf("missing library: %s, dlerror %s\n", h ? "found" : "NULL", dlerror() ? "set" : "unset");
	printf("dlerror after reading: %s\n", dlerror() ? "set" : "unset");

	void *self = dlopen(NULL, RTLD_NOW);
	printf("dlopen(NULL): %s\n", self ? "ok" : "NULL");
	void *s = dlsym(self, "no_such_symbol_xyz");
	printf("missing symbol: %s, dlerror %s\n", s ? "found" : "NULL", dlerror() ? "set" : "unset");

#ifdef __wasm__
	void *lib = dlopen("/usr/lib/wine/i386-unix/libtwice.so", RTLD_NOW);
	int (*f)(int) = lib ? (int (*)(int))dlsym(lib, "twice") : 0;
	int *d = lib ? dlsym(lib, "twice_data") : 0;
	int (*g)(int) = (int (*)(int))dlsym(RTLD_DEFAULT, "twice");
	int r = f && d && g == f ? f(*d) : -1;
	printf("registry: %d\n", r);
	printf("dlclose: %d\n", lib ? dlclose(lib) : -1);
#else
	printf("registry: %d\n", twice(twice_data));
	printf("dlclose: %d\n", 0);
#endif
	return 0;
}
