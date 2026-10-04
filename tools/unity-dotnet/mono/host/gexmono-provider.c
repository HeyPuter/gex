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

static void *
dl_load (const char *name, int flags, char **err, void *user_data)
{
	for (size_t i = 0; name && i < sizeof native_libs / sizeof native_libs [0]; i++)
		if (!strcmp (name, native_libs [i].lib))
			return (void *)&native_libs [i];
	uint64_t open = guest_fn ("dlopen", &guest_dlopen);
	if (!open)
		return NULL;
	uint64_t h = call2 (open, (uint64_t)(uintptr_t)name, GUEST_RTLD_LAZY);
	if (!h)
		return NULL;
	DlHandle *d = calloc (1, sizeof *d);
	d->guest = h;
	return d;
}

static void *
dl_symbol (void *handle, const char *name, char **err, void *user_data)
{
	const DlHandle *d = handle;
	const void *p = NULL;
	if (d->resolve)
		p = d->resolve (name);
	else if (guest_fn ("dlsym", &guest_dlsym))
		p = (const void *)(uintptr_t)call2 (guest_dlsym, d->guest, (uint64_t)(uintptr_t)name);
	if (!p && err)
		*err = strdup ("gexmono: symbol not found");
	return (void *)p;
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
	const char *dir = getenv ("GEXMONO_ASSEMBLIES");
	static const char *keys [] = { "System.Globalization.Invariant", "APP_CONTEXT_BASE_DIRECTORY" };
	const char *values [] = { "true", dir ? dir : "/" };
	monovm_initialize (2, keys, values);
	if (dir)
		mono_set_assemblies_path (dir);
	mono_wasm_install_interp_to_native_callback (gex_mono_interp_to_native);
	mono_dl_fallback_register (dl_load, dl_symbol, NULL, NULL);
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
GEXP_TABLE(gexp_mono_jit_init, gexp_mono_jit_init_version, gexp_mono_raise_exception);
