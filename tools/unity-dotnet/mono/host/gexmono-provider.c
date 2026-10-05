/*
 * gexmono-provider.c: upstream Mono (the r58Playz fork, wasm64, interpreter)
 * as a gex stateful provider in place of a guest's Mono library
 * (libmonobdwgc-2.0.so, mono-2.0-bdwgc.dll; docs/unity-dotnet.md).
 *
 * The guest calls the library's exports; gex binds each to `gexp_<name>`
 * (generated adapters, gen-adapters.py, plus the ones here). This file does
 * what the browser's driver does before mono_jit_init, minus JS: the
 * interp-to-native table, the BCL's native libraries linked in, the
 * interpreter. It also stands in for C++ exceptions, which a wasm module
 * without wasm EH cannot have (natively: gex.provider_try/throw).
 */
#include "config.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "gexmono-adapters.h"

void mono_icall_table_init (void);
void mono_ee_interp_init (const char *opts);
void mono_marshal_ilgen_init (void);
void mono_method_builder_ilgen_init (void);
void mono_sgen_mono_ilgen_init (void);
void mono_trace_init (void);
void *gex_mono_interp_to_native (char *cookie);
typedef void *(*MonoWasmNativeToInterpCallback) (char *cookie);
void mono_wasm_install_interp_to_native_callback (MonoWasmNativeToInterpCallback cb);
int monovm_initialize (int propertyCount, const char **propertyKeys, const char **propertyValues);
const void *SystemResolveDllImport (const char *name);

/* ---- P/Invokes: the BCL's native libraries linked into this module, and
 * everything else in the guest (its own dlopen/dlsym, as RTLD_DEFAULT for
 * "__Internal"). A guest function pointer is called by the guest's ABI
 * (interp.c's HOST_GEX path). ---- */

typedef const void *(*Resolver) (const char *);
typedef struct { const char *lib; Resolver resolve; uint64_t guest; } DlHandle;
static const DlHandle native_libs [] = {
	{ "libSystem.Native", SystemResolveDllImport, 0 },
};

__attribute__((import_module("gex"), import_name("provider_guest_sym"))) uint64_t gexp_host_guest_sym (const char *name, uint64_t len);

static uint64_t guest_dlopen, guest_dlsym;

static uint64_t
guest_fn (const char *name, uint64_t *cache)
{
	if (!*cache)
		*cache = gexp_host_guest_sym (name, strlen (name));
	return *cache;
}

static uint64_t
call2 (uint64_t fn, uint64_t a, uint64_t b)
{
	gexp_call c = { 0 };
	c.ireg [0] = a;
	c.ireg [1] = b;
	c.nireg = 2;
	gexp_call_guest (fn, &c);
	return c.rax;
}

#define GUEST_RTLD_LAZY 1

extern uint64_t gexmono_find_plugin, gexmono_dl_fallback [4];

static uint64_t
call4 (uint64_t fn, uint64_t a, uint64_t b, uint64_t c, uint64_t d)
{
	gexp_call k = { 0 };
	k.ireg [0] = a, k.ireg [1] = b, k.ireg [2] = c, k.ireg [3] = d;
	k.nireg = 4;
	gexp_call_guest (fn, &k);
	return k.rax;
}

static void *
guest_handle (uint64_t h, int unity)
{
	DlHandle *d = calloc (1, sizeof *d);
	d->guest = h;
	d->lib = unity ? "unity" : NULL;
	return d;
}

static void *
dl_load (const char *name, int flags, char **err, void *user_data)
{
	for (size_t i = 0; name && i < sizeof native_libs / sizeof native_libs [0]; i++)
		if (!strcmp (name, native_libs [i].lib))
			return (void *)&native_libs [i];
	/* Unity's find-plugin callback names a plugin's path. */
	const char *path = name;
	if (name && gexmono_find_plugin) {
		const char *p = (const char *)(uintptr_t)call2 (gexmono_find_plugin, (uint64_t)(uintptr_t)name, 0);
		if (p)
			path = p;
	}
	uint64_t open = guest_fn ("dlopen", &guest_dlopen);
	if (open) {
		uint64_t h = call2 (open, (uint64_t)(uintptr_t)path, GUEST_RTLD_LAZY);
		if (h)
			return guest_handle (h, 0);
	}
	/* Unity's own fallback: load (name, flags, &err, user_data). */
	if (gexmono_dl_fallback [0] && name) {
		char *e = NULL;
		uint64_t h = call4 (gexmono_dl_fallback [0], (uint64_t)(uintptr_t)name, flags, (uint64_t)(uintptr_t)&e, gexmono_dl_fallback [3]);
		if (h)
			return guest_handle (h, 1);
	}
	return NULL;
}

static void *
dl_symbol (void *handle, const char *name, char **err, void *user_data)
{
	const DlHandle *d = handle;
	const void *p = NULL;
	if (d->resolve) {
		p = d->resolve (name);
	} else if (d->lib && gexmono_dl_fallback [1]) {
		char *e = NULL;
		p = (const void *)(uintptr_t)call4 (gexmono_dl_fallback [1], d->guest, (uint64_t)(uintptr_t)name, (uint64_t)(uintptr_t)&e, gexmono_dl_fallback [3]);
	} else if (guest_fn ("dlsym", &guest_dlsym)) {
		p = (const void *)(uintptr_t)call2 (guest_dlsym, d->guest, (uint64_t)(uintptr_t)name);
	}
	if (!p && err)
		*err = strdup ("gexmono: symbol not found");
	return (void *)p;
}

/* ---- guest code is a GC-safe region ----
 * A thread attached to the runtime runs guest code in GC Safe (blocking)
 * mode, so a collection does not wait for it to poll: gex stops it at its
 * next safepoint instead (sgen-stw.c, gexp_stop_guest_threads), and its
 * guest stack and registers are scanned conservatively (sgen-mono.c). Each
 * crossing into guest code pushes whether it entered the region. */

#include <mono/utils/mono-threads.h>
#include <mono/utils/mono-threads-api.h>

#define GEXMONO_SAFE_MAX 256
static __thread gpointer safe_cookie [GEXMONO_SAFE_MAX];
static __thread int safe_depth;

void
gexp_hook_to_guest (void)
{
	gpointer cookie = NULL;
	MonoThreadInfo *info = mono_thread_info_current_unchecked ();
	if (info && mono_thread_info_current_state (info) == STATE_RUNNING) {
		gpointer dummy;
		cookie = mono_threads_enter_gc_safe_region_unbalanced (&dummy);
	}
	if (safe_depth < GEXMONO_SAFE_MAX)
		safe_cookie [safe_depth] = cookie;
	safe_depth++;
}

void
gexp_hook_from_guest (void)
{
	if (!safe_depth)
		return; /* the thread's first entry */
	safe_depth--;
	gpointer cookie = safe_depth < GEXMONO_SAFE_MAX ? safe_cookie [safe_depth] : NULL;
	if (cookie) {
		gpointer dummy;
		mono_threads_exit_gc_safe_region_unbalanced (cookie, &dummy);
	}
}

/* This is the embedding API's other side: its external-only functions are
 * ours to call (the runtime's headers make the warning an error). */
#pragma clang diagnostic ignored "-Wdeprecated-declarations"

/* ---- managed functions guest code calls through a pointer ----
 * A delegate's function pointer (Marshal.GetFunctionPointerForDelegate) or an
 * [UnmanagedCallersOnly] method's: the interpreter asks for a native entry to
 * its native-to-managed wrapper, and gets a reverse slot of the provider's
 * code region; guest code calling it enters gexp__reverse, which runs the
 * wrapper by the guest's ABI (interp.c gex_interp_native_to_managed). */

typedef struct { void *addr; void *arg; } GexFtnDesc; /* MonoFtnDesc's head */
typedef struct { MonoMethodSignature *sig; void *imethod; } GexReverse;
MonoMethodSignature *gex_interp_method_signature (void *imethod);
void gex_interp_native_to_managed (MonoMethodSignature *sig, void *imethod, gexp_call *c);
extern int gex_guest_abi_win64;
typedef void *(*MonoWasmGetNativeToInterpTramp) (MonoMethod *method, void *extra_arg);
void mono_wasm_install_get_native_to_interp_tramp (MonoWasmGetNativeToInterpTramp cb);

static void *
native_to_interp (MonoMethod *method, void *extra_arg)
{
	GexFtnDesc *ftn = extra_arg;
	if (ftn->arg == (void *)(intptr_t)-1)
		return NULL; /* an AOT body: not on this build */
	GexReverse *r = malloc (sizeof *r);
	r->imethod = ftn->arg;
	r->sig = gex_interp_method_signature (ftn->arg);
	return (void *)(uintptr_t)gexp_reverse_alloc (r);
}

GEXP_SYSV(gexp__reverse)
{
	uint64_t self = GEXP_X (ctx, GEXP_LAST_TARGET);
	gexp_enter (ctx);
	GexReverse *r = gexp_reverse_data (self);
	if (!r)
		__builtin_trap ();
	gexp_call c = { 0 };
	c.win64 = gex_guest_abi_win64;
	gexp_reverse_args (ctx, &c);
	gex_interp_native_to_managed (r->sig, r->imethod, &c);
	return gexp_reverse_return (ctx, ret, &c);
}

/* ---- raising a managed exception from guest code ---- */

GEXP_SYSV(gexp_mono_raise_exception)
{
	gexp_enter (ctx);
	MonoException *ex = (MonoException *)(uintptr_t)GEXP_ARG (ctx, 0);
	if (gexp_catching ())
		gexp_raise ((uint64_t)(uintptr_t)ex);
	mono_raise_exception (ex);
	__builtin_trap ();
}

/* ---- tracing ---- */

int gexmono_trace_on;

void
gexmono_trace (int ctx, const char *name)
{
	fprintf (stderr, "[gexmono] %s (%#llx, %#llx, %#llx)\n", name,
		(unsigned long long)gexp_arg (ctx, 0), (unsigned long long)gexp_arg (ctx, 1), (unsigned long long)gexp_arg (ctx, 2));
}

/* ---- before mono_jit_init ---- */

static void
preinit (void)
{
	static int done;
	if (done)
		return;
	done = 1;
	/* The fork's runtime wasm JIT: off until it emits wasm64 for gex. */
	setenv ("MONO_WASM_JIT_AUTO", "0", 0);
	gexmono_trace_on = getenv ("GEXMONO_TRACE") != NULL;
	const char *dir = getenv ("GEXMONO_ASSEMBLIES");
	static const char *keys [] = { "System.Globalization.Invariant", "APP_CONTEXT_BASE_DIRECTORY" };
	const char *values [] = { "true", dir ? dir : "/" };
	monovm_initialize (2, keys, values);
	if (dir)
		mono_set_assemblies_path (dir);
	mono_wasm_install_interp_to_native_callback (gex_mono_interp_to_native);
	mono_dl_fallback_register (dl_load, dl_symbol, NULL, NULL);
	mono_wasm_install_get_native_to_interp_tramp (native_to_interp);
	mono_jit_set_aot_mode (MONO_AOT_MODE_INTERP_ONLY);
	mono_icall_table_init ();
	mono_ee_interp_init ("-jiterp");
	mono_marshal_ilgen_init ();
	mono_method_builder_ilgen_init ();
	mono_sgen_mono_ilgen_init ();
	mono_trace_init ();
}

GEXP_SYSV(gexp_mono_jit_init)
{
	gexp_enter (ctx);
	preinit ();
	GEXP_RET_U64 (ctx, (uintptr_t)mono_jit_init ((const char *)GEXP_ARG (ctx, 0)));
	return gexp_return (ctx, ret);
}

GEXP_SYSV(gexp_mono_jit_init_version)
{
	gexp_enter (ctx);
	preinit ();
	GEXP_RET_U64 (ctx, (uintptr_t)mono_jit_init_version ((const char *)GEXP_ARG (ctx, 0), (const char *)GEXP_ARG (ctx, 1)));
	return gexp_return (ctx, ret);
}

/* ---- C++ exceptions, which this module cannot have ---- */

typedef void (*MonoLLVMInvokeCallback) (void *arg);

void
mono_llvm_cpp_throw_exception (void)
{
	gexp_host_throw (1);
}

void
mono_llvm_cpp_catch_exception (MonoLLVMInvokeCallback cb, void *arg, int *out_thrown)
{
	*out_thrown = gexp_host_try (cb, arg) != 0;
}

/* ---- the module's table ---- */

GEXP_DEFINE_MISSING
GEXP_DEFINE_THREAD_ENTRY
extern gexp_body_t const gexmono_generated_adapters [];
GEXP_TABLE(gexp_mono_jit_init, gexp_mono_jit_init_version, gexp_mono_raise_exception, gexp__reverse);
