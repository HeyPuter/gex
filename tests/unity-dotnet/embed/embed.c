/* Embeds Mono the way UnityPlayer does: dlopen the library, dlsym the
 * mono_* API, boot the runtime, load an assembly, invoke methods
 * (docs/unity-dotnet.md, P2/P3). Under gex the library is the stub
 * (monostub.c) and the wasm64 Mono provider serves it.
 *
 *   embed LIB ASSEMBLY */
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef void *P;
static P (*mono_jit_init_version) (const char *, const char *);
static P (*mono_domain_assembly_open) (P, const char *);
static P (*mono_assembly_get_image) (P);
static P (*mono_class_from_name) (P, const char *, const char *);
static P (*mono_class_get_method_from_name) (P, const char *, int);
static P (*mono_runtime_invoke) (P, P, void **, P *);
static P (*mono_string_new) (P, const char *);
static char *(*mono_string_to_utf8) (P);
static P (*mono_object_unbox) (P);
static P (*mono_object_to_string) (P, P *);
static void (*mono_gc_collect) (int);
static int (*mono_gc_max_generation) (void);
static P (*mono_class_get_name) (P);
static void (*mono_add_internal_call) (const char *, const void *);
static void (*mono_raise_exception) (P);
static P (*mono_get_exception_argument) (const char *, const char *);
static P (*mono_get_root_domain) (void);
static P (*mono_thread_attach) (P);

/* A thread of the program's own (as Unity's job workers are): attached,
 * calling into managed code, holding the objects it gets only in its frame
 * while it runs guest code and the main thread collects. */
static P g_make;
static volatile int worker_bad = -1;
static void *
worker (void *arg)
{
	mono_thread_attach (mono_get_root_domain ());
	int bad = 0;
	for (int round = 0; round < 20; round++) {
		P held [32];
		P exc = NULL;
		for (int i = 0; i < 32; i++) {
			int v = 1000 + round * 32 + i;
			void *a [1] = { &v };
			held [i] = mono_runtime_invoke (g_make, NULL, a, &exc);
		}
		volatile unsigned long spin = 0;
		for (unsigned long k = 0; k < 2000000; k++)
			spin += k ^ (spin >> 3);
		for (int i = 0; i < 32; i++) {
			int v = 1000 + round * 32 + i;
			char want [128];
			snprintf (want, sizeof want, "object %d %.*s", v, v % 50, "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
			if (strcmp (mono_string_to_utf8 (held [i]), want))
				bad++;
		}
	}
	worker_bad = bad;
	return NULL;
}

/* Internal calls (registered with mono_add_internal_call) and P/Invoke
 * targets ("__Internal": this program's exports), as UnityPlayer's are. */
static int Twice (int x) { return 2 * x; }
static P Concat (P a, P b)
{
	char buf [256];
	snprintf (buf, sizeof buf, "%s%s", mono_string_to_utf8 (a), mono_string_to_utf8 (b));
	return mono_string_new (mono_get_root_domain (), buf);
}
static void Throws (P msg)
{
	volatile int keep = 7; /* a frame of its own that the raise abandons */
	mono_raise_exception (mono_get_exception_argument (NULL, mono_string_to_utf8 (msg)));
	printf ("not reached %d\n", keep);
}
typedef struct { float x, y, z; } Vec3;
typedef struct { long a, b, c; double d; } Big;
__attribute__((visibility("default"))) double gex_mix (int a, double b, Vec3 v, long c, float f)
{
	return a * b + v.x + 10 * v.y + 100 * v.z + (double)(c >> 40) + f;
}
__attribute__((visibility("default"))) Vec3 gex_scale (Vec3 v, float k)
{
	return (Vec3){ v.x * k, v.y * k, v.z * k };
}
__attribute__((visibility("default"))) Big gex_big (long a, Big b, long c, long d, long e, long f, long g, long h)
{
	return (Big){ b.a + a, b.b + c + d, b.c + e + f + g + h, b.d * 2 };
}

#define SYM(n) do { *(void **)&n = dlsym (h, #n); if (!n) { printf ("no %s\n", #n); return 1; } } while (0)

static void
check (const char *what, P exc)
{
	if (!exc)
		return;
	P s = mono_object_to_string (exc, NULL);
	printf ("%s threw: %s\n", what, s ? mono_string_to_utf8 (s) : "?");
	exit (1);
}

int
main (int argc, char **argv)
{
	void *h = dlopen (argv [1], RTLD_NOW);
	if (!h) { printf ("dlopen: %s\n", dlerror ()); return 1; }
	SYM (mono_jit_init_version); SYM (mono_domain_assembly_open); SYM (mono_assembly_get_image);
	SYM (mono_class_from_name); SYM (mono_class_get_method_from_name); SYM (mono_runtime_invoke);
	SYM (mono_string_new); SYM (mono_string_to_utf8); SYM (mono_object_unbox); SYM (mono_object_to_string);
	SYM (mono_gc_collect); SYM (mono_gc_max_generation); SYM (mono_class_get_name);
	SYM (mono_add_internal_call); SYM (mono_raise_exception); SYM (mono_get_exception_argument); SYM (mono_get_root_domain); SYM (mono_thread_attach);
	P dom = mono_jit_init_version ("embed", NULL);
	printf ("init: %s\n", dom ? "ok" : "failed");
	P a = mono_domain_assembly_open (dom, argv [2]);
	if (!a) { printf ("assembly open failed\n"); return 1; }
	P k = mono_class_from_name (mono_assembly_get_image (a), "", "T");
	printf ("class %s\n", (char *)mono_class_get_name (k));
	P compute = mono_class_get_method_from_name (k, "Compute", 1);
	P hello = mono_class_get_method_from_name (k, "Hello", 1);
	P guest = mono_class_get_method_from_name (k, "Guest", 0);
	mono_add_internal_call ("T::Twice", Twice);
	mono_add_internal_call ("T::Concat", Concat);
	mono_add_internal_call ("T::Throws", Throws);
	int n = 50;
	void *args [1] = { &n };
	P exc = NULL;
	P r = mono_runtime_invoke (compute, NULL, args, &exc);
	check ("Compute", exc);
	printf ("Compute(50) = %d\n", *(int *)mono_object_unbox (r));
	void *hargs [1] = { mono_string_new (dom, "gex") };
	P hr = mono_runtime_invoke (hello, NULL, hargs, &exc);
	check ("Hello", exc);
	printf ("Hello = %s\n", mono_string_to_utf8 (hr));
	P gr = mono_runtime_invoke (guest, NULL, NULL, &exc);
	check ("Guest", exc);
	printf ("Guest = %s\n", mono_string_to_utf8 (gr));
	/* Objects held only in this frame (guest stack and registers) across
	 * collections: the provider's GC scans the guest's frames. */
	P make = mono_class_get_method_from_name (k, "Make", 1);
	P churn = mono_class_get_method_from_name (k, "Churn", 1);
	P held [64];
	for (int i = 0; i < 64; i++) {
		void *margs [1] = { &i };
		held [i] = mono_runtime_invoke (make, NULL, margs, &exc);
		check ("Make", exc);
	}
	int rounds = 5;
	void *cargs [1] = { &rounds };
	mono_runtime_invoke (churn, NULL, cargs, &exc);
	check ("Churn", exc);
	int bad = 0;
	for (int i = 0; i < 64; i++) {
		char want [128];
		snprintf (want, sizeof want, "object %d %.*s", i, i % 50, "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
		if (strcmp (mono_string_to_utf8 (held [i]), want))
			bad++;
	}
	printf ("held across collections: %d of 64 intact\n", 64 - bad);
	g_make = make;
	pthread_t t;
	pthread_create (&t, NULL, worker, NULL);
	for (int r = 0; r < 4; r++) {
		mono_runtime_invoke (churn, NULL, cargs, &exc);
		check ("Churn", exc);
	}
	pthread_join (t, NULL);
	printf ("worker thread: %d bad of 640\n", worker_bad);
	mono_gc_collect (mono_gc_max_generation ());
	printf ("gc ok\n");
	return 0;
}
