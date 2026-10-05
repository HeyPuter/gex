/*
 * gexmono.c: bring-up of upstream Mono (r58Playz/dotnet-runtime wasm-jspi-jit-10.0.3, built for
 * wasm64) outside the browser's JS runtime. What the browser driver (src/mono/browser/runtime) does,
 * minus JS: the interp-to-native table, DllImports of the BCL's native libraries linked statically,
 * runtime properties, the interpreter.
 */
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#include <mono/jit/jit.h>
#include <mono/metadata/assembly.h>
#include <mono/utils/mono-dl-fallback.h>

int monovm_initialize (int propertyCount, const char **propertyKeys, const char **propertyValues);
void mono_icall_table_init (void);
void mono_ee_interp_init (const char *opts);
void mono_marshal_ilgen_init (void);
void mono_method_builder_ilgen_init (void);
void mono_sgen_mono_ilgen_init (void);
void mono_trace_init (void);
void *gex_mono_interp_to_native (char *cookie);
typedef void *(*MonoWasmNativeToInterpCallback) (char *cookie);
void mono_wasm_install_interp_to_native_callback (MonoWasmNativeToInterpCallback cb);
const void *SystemResolveDllImport (const char *name);

typedef const void *(*Resolver) (const char *);
static const struct { const char *lib; Resolver resolve; } native_libs [] = {
	{ "libSystem.Native", SystemResolveDllImport },
};

static void *
dl_load (const char *name, int flags, char **err, void *user_data)
{
	if (!name)
		return NULL;
	for (size_t i = 0; i < sizeof native_libs / sizeof native_libs [0]; i++)
		if (!strcmp (name, native_libs [i].lib))
			return (void *)&native_libs [i];
	return NULL;
}

static void *
dl_symbol (void *handle, const char *name, char **err, void *user_data)
{
	const void *p = ((typeof (&native_libs [0]))handle)->resolve (name);
	if (!p && err)
		*err = strdup ("gexmono: symbol not found");
	return (void *)p;
}

#define STEP(x) do { if (getenv ("GEXMONO_TRACE")) fprintf (stderr, "gexmono: %s\n", #x); x; } while (0)

void
gex_mono_init (const char *assemblies_path)
{
	/* The fork's runtime wasm JIT emits wasm32 code through JS; off until it emits wasm64 for gex. */
	STEP (setenv ("MONO_WASM_JIT_AUTO", "0", 1));
	static const char *keys [] = { "System.Globalization.Invariant", "APP_CONTEXT_BASE_DIRECTORY" };
	const char *values [] = { "true", assemblies_path };
	STEP (monovm_initialize (2, keys, values));
	STEP (mono_set_assemblies_path (assemblies_path));
	STEP (mono_wasm_install_interp_to_native_callback (gex_mono_interp_to_native));
	STEP (mono_dl_fallback_register (dl_load, dl_symbol, NULL, NULL));
	STEP (mono_jit_set_aot_mode (MONO_AOT_MODE_INTERP_ONLY));
	STEP (mono_icall_table_init ());
	STEP (mono_ee_interp_init ("-jiterp"));
	STEP (mono_marshal_ilgen_init ());
	STEP (mono_method_builder_ilgen_init ());
	STEP (mono_sgen_mono_ilgen_init ());
	STEP (mono_trace_init ());
}
