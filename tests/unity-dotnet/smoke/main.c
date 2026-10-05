// Smoke test: upstream Mono (r58 fork) built for wasm64, interpreter, CoreLib load,
// assembly load, class/method lookup, an internal call, runtime_invoke, strings.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <mono/jit/jit.h>
#include <mono/metadata/assembly.h>
#include <mono/metadata/appdomain.h>
#include <mono/metadata/class.h>
#include <mono/metadata/object.h>
#include <mono/metadata/loader.h>
#include <mono/metadata/mono-gc.h>
#include <mono/utils/mono-logger.h>

void mono_icall_table_init (void);
void mono_ee_interp_init (const char *opts);
void mono_marshal_ilgen_init (void);
void mono_method_builder_ilgen_init (void);
void mono_sgen_mono_ilgen_init (void);
void mono_trace_init (void);

void *gex_mono_interp_to_native (char *cookie);
typedef void *(*MonoWasmNativeToInterpCallback) (char *cookie);
void mono_wasm_install_interp_to_native_callback (MonoWasmNativeToInterpCallback cb);

void gex_mono_init (const char *assemblies_path);
static int native_calls;
static int Native (int x) { native_calls++; return x * 1000; }

static void
check_exc (const char *what, MonoObject *exc)
{
	if (!exc) return;
	MonoString *s = mono_object_to_string (exc, NULL);
	char *m = s ? mono_string_to_utf8 (s) : strdup ("?");
	printf ("%s threw: %s\n", what, m);
	exit (1);
}

int
main (int argc, char **argv)
{
	const char *libdir = argv [1], *asmpath = argv [2];
	for (int i = 3; i < argc; i++) { char *e = strdup (argv [i]), *q = strchr (e, '='); if (q) { *q = 0; setenv (e, q + 1, 1); } }
	gex_mono_init (libdir);
	MonoDomain *dom = mono_jit_init_version ("smoke", NULL);
	printf ("init ok: domain %p, sizeof(void*)=%d\n", (void *)dom, (int)sizeof (void *));
	mono_add_internal_call ("T::Native", (const void *)Native);

	MonoAssembly *a = mono_domain_assembly_open (dom, asmpath);
	if (!a) { printf ("assembly open failed\n"); return 1; }
	MonoImage *img = mono_assembly_get_image (a);
	MonoClass *k = mono_class_from_name (img, "", "T");
	MonoMethod *run = mono_class_get_method_from_name (k, "Run", 1);
	MonoMethod *hello = mono_class_get_method_from_name (k, "Hello", 1);
	printf ("class %p run %p hello %p\n", (void *)k, (void *)run, (void *)hello);

	int n = 50;
	void *args [1] = { &n };
	MonoObject *exc = NULL;
	MonoObject *r = mono_runtime_invoke (run, NULL, args, &exc);
	check_exc ("Run", exc);
	printf ("Run(50) = %d (expect 50002), icall calls %d\n", *(int *)mono_object_unbox (r), native_calls);

	MonoString *s = mono_string_new (dom, "gex");
	void *hargs [1] = { s };
	MonoObject *hr = mono_runtime_invoke (hello, NULL, hargs, &exc);
	check_exc ("Hello", exc);
	printf ("Hello = %s\n", mono_string_to_utf8 ((MonoString *)hr));
	mono_gc_collect (mono_gc_max_generation ());
	printf ("gc ok, heap %lld\n", (long long)mono_gc_get_heap_size ());
	fflush (stdout);
	return 0;
}
