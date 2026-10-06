/*
 * gexmono-unity.c: the parts of Unity's Mono embedding API that upstream Mono
 * lacks or has without a callback the guest can give it (docs/unity-dotnet.md
 * §2): the mono_unity_* functions of Unity's fork, the legacy names Unity
 * still looks up, and the functions taking guest callbacks. Semantics follow
 * Unity-Technologies/mono (unity-utils.c and friends); what only serves
 * Unity's memory profiler reports nothing.
 *
 * Each is a gex F_body adapter (gexprov64.h), listed in gexmono_unity_adapters.
 */
#include "config.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include "gexmono-adapters.h"
#include <mono/metadata/class-internals.h>
#include <mono/metadata/object-internals.h>
#include <mono/metadata/metadata-internals.h>
#include <mono/metadata/domain-internals.h>
#include <mono/metadata/assembly-internals.h>
#include <mono/metadata/custom-attrs-internals.h>
#include <mono/metadata/reflection-internals.h>
#include <mono/utils/mono-error-internals.h>
#include <mono/utils/mono-logger-internals.h>

#pragma clang diagnostic ignored "-Wdeprecated-declarations"

void *gex_class_get_userdata (MonoClass *klass);
void gex_class_set_userdata (MonoClass *klass, void *userdata);
int gex_class_get_userdata_offset (void);
gboolean gex_class_is_inited (MonoClass *klass);
guint32 gex_class_get_data_size (MonoClass *klass);

#define A(i) gexp_arg (ctx, i)
#define P(i) ((void *)(uintptr_t)A (i))
#define U(name) GEXP_SYSV (gexp_##name)
#define ENTER do { gexp_enter (ctx); GEXMONO_TRACE (__func__ + 5); } while (0)
#define RET(v) do { GEXP_RET_U64 (ctx, (uint64_t)(v)); return gexp_return (ctx, ret); } while (0)
#define RETV return gexp_return (ctx, ret)

/* A guest callback: call it by the guest's ABI with integer arguments. */
static uint64_t
guest_cb (uint64_t fn, int n, uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3, uint64_t a4)
{
	gexp_call c = { 0 };
	uint64_t a [5] = { a0, a1, a2, a3, a4 };
	for (int i = 0; i < n; i++)
		c.ireg [i] = a [i];
	c.nireg = n;
	gexp_call_guest (fn, &c);
	return c.rax;
}

/* ---- the object layout Unity reads directly ---- */

U(mono_unity_object_header_size) { ENTER; RET (sizeof (MonoObject)); }
U(mono_unity_array_object_header_size) { ENTER; RET (offsetof (MonoArray, vector)); }
U(mono_unity_offset_of_array_length_in_array_object_header) { ENTER; RET (offsetof (MonoArray, max_length)); }
U(mono_unity_offset_of_array_bounds_in_array_object_header) { ENTER; RET (offsetof (MonoArray, bounds)); }
U(mono_unity_allocation_granularity) { ENTER; RET (2 * sizeof (void *)); }

/* ---- classes, types, fields, methods ---- */

U(mono_class_get_userdata) { ENTER; RET (gex_class_get_userdata (P (0))); }
U(mono_class_set_userdata) { ENTER; gex_class_set_userdata (P (0), P (1)); RETV; }
U(mono_class_get_userdata_offset) { ENTER; RET ((uint32_t)gex_class_get_userdata_offset ()); }
U(mono_class_is_blittable) { ENTER; RET (m_class_is_blittable ((MonoClass *)P (0))); }
U(mono_class_is_generic) { ENTER; RET (mono_class_is_gtd ((MonoClass *)P (0))); }
U(mono_class_is_inflated) { ENTER; RET (mono_class_is_ginst ((MonoClass *)P (0))); }
U(mono_unity_class_is_interface) { ENTER; RET (MONO_CLASS_IS_INTERFACE_INTERNAL ((MonoClass *)P (0))); }
U(mono_unity_class_is_abstract) { ENTER; RET ((mono_class_get_flags ((MonoClass *)P (0)) & TYPE_ATTRIBUTE_ABSTRACT) != 0); }
U(mono_unity_class_is_inited) { ENTER; RET (gex_class_is_inited (P (0))); }
U(mono_unity_class_has_failure) { ENTER; RET (mono_class_has_failure ((MonoClass *)P (0))); }
U(mono_unity_class_get_data_size) { ENTER; RET (gex_class_get_data_size (P (0))); }
U(mono_unity_class_is_open_constructed_type) { ENTER; RET (mono_class_is_open_constructed_type (m_class_get_byval_arg ((MonoClass *)P (0)))); }
U(mono_unity_class_try_get_vtable) { ENTER; RET (mono_class_try_get_vtable ((MonoClass *)P (1))); }
U(mono_unity_class_field_is_literal) { ENTER; RET ((((MonoClassField *)P (0))->type->attrs & FIELD_ATTRIBUTE_LITERAL) != 0); }
U(mono_unity_class_get_generic_type_definition)
{
	ENTER;
	MonoClass *k = P (0);
	RET (mono_class_is_ginst (k) ? mono_class_get_generic_class (k)->container_class : NULL);
}
U(mono_unity_class_get) { ENTER; ERROR_DECL (e); MonoClass *k = mono_class_get_checked (P (0), (guint32)A (1), e); mono_error_cleanup (e); RET (k); }
U(mono_unity_class_get_checked) { ENTER; RET (mono_class_get_checked (P (0), (guint32)A (1), P (2))); }
U(mono_unity_field_from_token_checked) { ENTER; RET (mono_field_from_token_checked (P (0), (guint32)A (1), P (2), P (3), P (4))); }
U(mono_unity_get_method_checked) { ENTER; RET (mono_get_method_checked (P (0), (guint32)A (1), P (2), P (3), P (4))); }
U(mono_unity_type_is_pointer_type) { ENTER; RET (((MonoType *)P (0))->type == MONO_TYPE_PTR); }
U(mono_unity_type_is_static) { ENTER; RET ((((MonoType *)P (0))->attrs & FIELD_ATTRIBUTE_STATIC) != 0); }
U(mono_type_get_attrs) { ENTER; RET (((MonoType *)P (0))->attrs); }
U(mono_unity_vtable_get_static_field_data) { ENTER; RET (mono_vtable_get_static_field_data (P (0))); }
U(mono_method_signature_checked) { ENTER; RET (mono_method_signature_checked (P (0), P (1))); }
U(mono_method_signature_checked_slow) { ENTER; RET (mono_method_signature_checked_slow (P (0), P (1))); }
U(mono_unity_type_get_name_full_chunked)
{
	ENTER;
	char *name = mono_type_get_name_full (P (0), MONO_TYPE_NAME_FORMAT_IL);
	if (name) {
		guest_cb (A (1), 2, (uintptr_t)name, A (2), 0, 0, 0);
		g_free (name);
	}
	RETV;
}

/* Unity's mscorlib is .NET Framework's reference source, whose private
 * fields are m_name; .NET's CoreLib names most of the same fields _name.
 * UnityPlayer reads a few of them by name (AttributeUsageAttribute's
 * m_attributeTarget, m_inherited, m_allowMultiple). A miss on m_x retries
 * _x, then the table; a final miss is logged. */
static const char *const field_alias [][3] = {
	{ NULL, NULL, NULL },
};
U(mono_class_get_field_from_name)
{
	ENTER;
	MonoClass *k = P (0);
	const char *name = P (1);
	MonoClassField *f = mono_class_get_field_from_name (k, name);
	if (!f && name && name [0] == 'm' && name [1] == '_') {
		char *alt = g_strdup (name + 1);
		f = mono_class_get_field_from_name (k, alt);
		g_free (alt);
	}
	for (int i = 0; !f && name && field_alias [i][0]; i++)
		if (!strcmp (name, field_alias [i][1]) && !strcmp (m_class_get_name (k), field_alias [i][0]))
			f = mono_class_get_field_from_name (k, field_alias [i][2]);
	if (!f && k)
		fprintf (stderr, "[gexmono] no field %s.%s::%s\n", m_class_get_name_space (k), m_class_get_name (k), name ? name : "(null)");
	RET (f);
}

/* mono_class_is_subclass_of (klass, klassc, check_interfaces). GEXMONO_TRACE_SUBCLASS=NAME logs
 * every test against a class called NAME (Unity's FindObjectOfType walks every object this way) with
 * both classes' images, so a type loaded twice (two MonoClass for one name) shows. */
U(mono_class_is_subclass_of)
{
	ENTER;
	MonoClass *k = P (0), *c = P (1);
	gboolean r = mono_class_is_subclass_of (k, c, (gboolean)A (2));
	static const char *want; static int init;
	if (!init) { want = getenv ("GEXMONO_TRACE_SUBCLASS"); init = 1; }
	if (want && k && c && (!strcmp (m_class_get_name (c), want) || !strcmp (m_class_get_name (k), want)))
		fprintf (stderr, "[gexmono] subclass %s.%s (%p, image %p %s) of %s.%s (%p, image %p %s) = %d\n",
			m_class_get_name_space (k), m_class_get_name (k), k, m_class_get_image (k), m_class_get_image (k)->name,
			m_class_get_name_space (c), m_class_get_name (c), c, m_class_get_image (c), m_class_get_image (c)->name, r);
	RET (r);
}

/* mono_class_get_methods (klass, iter): with GEXMONO_TRACE_METHODS=1, a line for each
 * finished pass over a class (its name, how many methods, how many passes so far):
 * which class Unity scans over and over. */
U(mono_class_get_methods)
{
	ENTER;
	MonoClass *k = P (0);
	gpointer *iter = P (1);
	MonoMethod *m = mono_class_get_methods (k, iter);
	static int on = -1;
	if (on < 0)
		on = getenv ("GEXMONO_TRACE_METHODS") ? atoi (getenv ("GEXMONO_TRACE_METHODS")) : 0;
	if (on >= 2 && m) {
		ERROR_DECL (err);
		MonoMethodSignature *sig = mono_method_signature_checked (m, err);
		fprintf (stderr, "[gexmono]   %s.%s::%s %p sig %p%s%s\n", m_class_get_name_space (k), m_class_get_name (k), m->name, m, sig,
			is_ok (err) ? "" : " error ", is_ok (err) ? "" : mono_error_get_message (err));
		mono_error_cleanup (err);
	}
	if (on && !m && k) {
		static GHashTable *passes;
		static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
		pthread_mutex_lock (&lock);
		if (!passes)
			passes = g_hash_table_new (NULL, NULL);
		guint n = GPOINTER_TO_UINT (g_hash_table_lookup (passes, k)) + 1;
		g_hash_table_insert (passes, k, GUINT_TO_POINTER (n));
		pthread_mutex_unlock (&lock);
		if (n <= 3 || (n & (n - 1)) == 0)
			fprintf (stderr, "[gexmono] methods of %s.%s (%p, %d methods): pass %u\n", m_class_get_name_space (k),
				m_class_get_name (k), k, mono_class_get_method_count (k), n);
	}
	RET (m);
}

/* GEXMONO_TRACE_ARRAYS=1: each mono_array_new with its class, length and result, and any
 * mono_gc_wbarrier_set_field after which the slot does not hold the value (diagnostics for
 * the Instantiate crash, docs/unity-dotnet.md §12). */
static int trace_arrays = -1;
static int
arrays_on (void)
{
	if (trace_arrays < 0)
		trace_arrays = getenv ("GEXMONO_TRACE_ARRAYS") != NULL;
	return trace_arrays;
}
U(mono_array_new)
{
	ENTER;
	MonoArray *a = mono_array_new ((MonoDomain *)P (0), (MonoClass *)P (1), (uintptr_t)A (2));
	if (arrays_on ())
		fprintf (stderr, "[gexmono] array_new %s.%s[%llu] = %p\n", P (1) ? m_class_get_name_space ((MonoClass *)P (1)) : "?",
			P (1) ? m_class_get_name ((MonoClass *)P (1)) : "?", (unsigned long long)A (2), a);
	RET (a);
}
U(mono_gc_wbarrier_set_field)
{
	ENTER;
	void *slot = P (1);
	MonoObject *v = P (2);
	mono_gc_wbarrier_set_field ((MonoObject *)P (0), slot, v);
	if (arrays_on () && *(MonoObject **)slot != v)
		fprintf (stderr, "[gexmono] wbarrier: slot %p holds %p after storing %p\n", slot, *(MonoObject **)slot, v);
	RETV;
}

/* mono_custom_attrs_from_field (klass, field): Mono finds the field's
 * metadata row from its index in klass's field array, so klass must be the
 * class that defines the field. UnityPlayer's serializer passes the class it
 * is serializing, which for an inherited field is a subclass; use the
 * field's own parent. */
U(mono_custom_attrs_from_field)
{
	ENTER;
	MonoClass *k = P (0);
	MonoClassField *f = P (1);
	MonoClass *parent = f ? m_field_get_parent (f) : NULL;
	if (parent && parent != k) {
		static int logged;
		if (gexmono_trace_on || logged++ < 8)
			fprintf (stderr, "[gexmono] custom_attrs_from_field: %s::%s is %s's, not %s's\n", m_class_get_name (parent),
				mono_field_get_name (f), m_class_get_name (parent), k ? m_class_get_name (k) : "?");
		k = parent;
	}
	RET (mono_custom_attrs_from_field (k, f));
}

U(unity_mono_method_is_generic) { ENTER; RET (((MonoMethod *)P (0))->is_generic); }
U(unity_mono_method_is_inflated) { ENTER; RET (((MonoMethod *)P (0))->is_inflated); }
U(unity_mono_reflection_method_get_method) { ENTER; MonoReflectionMethod *m = P (0); RET (m ? m->method : NULL); }

/* ---- custom attributes ---- */

U(mono_custom_attrs_get_attrs)
{
	ENTER;
	MonoCustomAttrInfo *ainfo = P (0);
	void **iter = P (1);
	MonoClass *r = NULL;
	if (iter && ainfo->num_attrs) {
		int index = GPOINTER_TO_INT (*iter);
		if (index < ainfo->num_attrs) {
			r = ainfo->attrs [index].ctor->klass;
			*iter = GINT_TO_POINTER (index + 1);
		}
	}
	RET (r);
}
U(mono_custom_attrs_construct_by_type) { ENTER; RET (mono_custom_attrs_construct (P (0))); }
U(mono_unity_custom_attrs_construct) { ENTER; RET (mono_custom_attrs_construct (P (0))); }

/* ---- arrays, strings, errors ---- */

U(mono_unity_array_new_2d)
{
	ENTER;
	uintptr_t lengths [2] = { A (2), A (3) };
	ERROR_DECL (e);
	MonoArray *r = mono_array_new_full_checked (mono_class_create_array (P (1), 2), lengths, NULL, e);
	mono_error_cleanup (e);
	RET (r);
}
U(mono_unity_array_new_3d)
{
	ENTER;
	uintptr_t lengths [3] = { A (2), A (3), A (4) };
	ERROR_DECL (e);
	MonoArray *r = mono_array_new_full_checked (mono_class_create_array (P (1), 3), lengths, NULL, e);
	mono_error_cleanup (e);
	RET (r);
}
U(mono_unity_string_empty_wrapper) { ENTER; RET (mono_string_empty_internal (mono_domain_get ())); }
U(mono_unity_error_convert_to_exception) { ENTER; RET (mono_error_convert_to_exception (P (0))); }
U(mono_unity_loader_get_last_error_and_error_prepare_exception) { ENTER; RET (0); }
U(mono_unity_g_free) { ENTER; g_free (P (0)); RETV; }
U(mono_assembly_name_parse) { ENTER; RET (mono_assembly_name_parse (P (0), P (1))); }

/* ---- runtime set-up Unity does, single-domain on .NET ---- */

static char *data_dir;
U(mono_unity_set_data_dir) { ENTER; g_free (data_dir); data_dir = g_strdup (P (0)); RETV; }
U(mono_unity_set_embeddinghostname) { ENTER; RETV; }
U(mono_unity_domain_set_config) { ENTER; RETV; }
U(mono_unity_runtime_set_main_args) { ENTER; mono_runtime_set_main_args ((int)A (0), P (1)); RETV; }
U(mono_unity_install_memory_callbacks) { ENTER; RETV; } /* Mono keeps its own allocator */
U(mono_unity_install_unitytls_interface) { ENTER; RETV; } /* Mono's TLS stack is not .NET's */
U(mono_unity_set_vprintf_func) { ENTER; RETV; }
U(mono_unity_jit_cleanup) { ENTER; RETV; }
U(mono_set_defaults) { ENTER; RETV; }
U(mono_parse_default_optimizations) { ENTER; RET (0); }
U(mono_verifier_set_mode) { ENTER; RETV; }
U(mono_set_ignore_version_and_key_when_finding_assemblies_already_loaded) { ENTER; RETV; }
U(mono_runtime_unhandled_exception_policy_set) { ENTER; RETV; }
U(mono_threads_set_shutting_down) { ENTER; RETV; }
U(mono_thread_suspend_all_other_threads) { ENTER; RETV; }
U(mono_thread_pool_cleanup) { ENTER; RETV; }
U(mono_thread_push_appdomain_ref) { ENTER; RETV; }
U(mono_thread_pop_appdomain_ref) { ENTER; RETV; }
U(mono_thread_has_sufficient_execution_stack) { ENTER; RET (1); }
U(mono_gchandle_is_in_domain) { ENTER; RET (1); }
U(mono_gchandle_is_in_domain_v2) { ENTER; RET (1); }
/* Unity 6 uses the v2 handle API with Unity's Mono encoding: a v2 handle is the GC's own handle value
 * (slot << 3 | type + 1), which Unity's libmonobdwgc also returns from the v1 calls. Its scripting layer
 * tests bit 0: set (normal and weak handles), a GC handle; clear, a pointer to an object reference; and its
 * managed code takes the same values (GCHandle.FromIntPtr). This tree's runtime and CoreLib use that raw
 * value as the handle (gc-internals.h, GCHandle.cs), so v1 and v2 calls are the same functions here. */
MonoGCHandle mono_gchandle_new_internal (MonoObject *obj, gboolean pinned);
MonoGCHandle mono_gchandle_new_weakref_internal (MonoObject *obj, gboolean track_resurrection);
MonoObject *mono_gchandle_get_target_internal (MonoGCHandle gchandle);
void mono_gchandle_free_internal (MonoGCHandle gchandle);
#define UNITY_H(h) ((uintptr_t)(h))
#define GC_H(u) ((MonoGCHandle)(uintptr_t)(u))
/* Unity reads handles after freeing them (Hollow Knight: get_target on a freed handle), which Unity's
 * Boehm-backed Mono answers with NULL and SGen asserts on. So the handles given out are remembered, and a
 * read or free of one that is not live is NULL / nothing. */
static GHashTable *live_handles;
static pthread_mutex_t live_lock = PTHREAD_MUTEX_INITIALIZER;
static uintptr_t
handle_out (MonoGCHandle h)
{
	uintptr_t u = UNITY_H (h);
	pthread_mutex_lock (&live_lock);
	if (!live_handles)
		live_handles = g_hash_table_new (NULL, NULL);
	g_hash_table_add (live_handles, (gpointer)u);
	pthread_mutex_unlock (&live_lock);
	if (gexmono_trace_on)
		fprintf (stderr, "[gexmono] -> gchandle %#llx\n", (unsigned long long)u);
	return u;
}
static gboolean
handle_live (uintptr_t u, gboolean drop)
{
	pthread_mutex_lock (&live_lock);
	gboolean live = live_handles && (drop ? g_hash_table_remove (live_handles, (gpointer)u) : g_hash_table_contains (live_handles, (gpointer)u));
	pthread_mutex_unlock (&live_lock);
	if (!live)
		fprintf (stderr, "[gexmono] gchandle %#llx is not live (%s)\n", (unsigned long long)u, drop ? "free" : "get_target");
	return live;
}
U(mono_gchandle_new_v2) { ENTER; RET (handle_out (mono_gchandle_new_internal ((MonoObject *)P (0), (gboolean)A (1)))); }
U(mono_gchandle_new_weakref_v2) { ENTER; RET (handle_out (mono_gchandle_new_weakref_internal ((MonoObject *)P (0), (gboolean)A (1)))); }
U(mono_gchandle_get_target_v2) { ENTER; RET (handle_live (A (0), FALSE) ? (uintptr_t)mono_gchandle_get_target_internal (GC_H (A (0))) : 0); }
U(mono_gchandle_free_v2) { ENTER; if (handle_live (A (0), TRUE)) mono_gchandle_free_internal (GC_H (A (0))); RETV; }
/* The v1 calls too, in the same encoding: Unity mixes them (a handle from one, freed or read by the other). */
U(mono_gchandle_new) { ENTER; RET ((uint32_t)handle_out (mono_gchandle_new_internal ((MonoObject *)P (0), (gboolean)A (1)))); }
U(mono_gchandle_new_weakref) { ENTER; RET ((uint32_t)handle_out (mono_gchandle_new_weakref_internal ((MonoObject *)P (0), (gboolean)A (1)))); }
U(mono_gchandle_get_target) { ENTER; RET (handle_live ((uint32_t)A (0), FALSE) ? (uintptr_t)mono_gchandle_get_target_internal (GC_H ((uint32_t)A (0))) : 0); }
U(mono_gchandle_free) { ENTER; if (handle_live ((uint32_t)A (0), TRUE)) mono_gchandle_free_internal (GC_H ((uint32_t)A (0))); RETV; }
U(mono_pmip) { ENTER; RET (0); }
U(mono_debug_lookup_source_location_by_il) { ENTER; RET (0); }

/* ---- the GC: Boehm-like, never incremental ---- */

static int gc_mode; /* MONO_GC_MODE_ENABLED 0, DISABLED 1, MANUAL 2 (Unity's) */
U(mono_unity_gc_set_mode) { ENTER; gc_mode = (int)A (0); RETV; }
U(mono_unity_gc_disable) { ENTER; gc_mode = 1; RETV; }
U(mono_unity_gc_enable) { ENTER; gc_mode = 0; RETV; }
U(mono_unity_gc_is_disabled) { ENTER; RET (gc_mode == 1); }
U(mono_gc_is_incremental) { ENTER; RET (0); }
U(mono_gc_set_incremental) { ENTER; RETV; }
U(mono_gc_collect_a_little) { ENTER; RET (0); }
U(mono_gc_get_max_time_slice_ns) { ENTER; RET (0); }
U(mono_gc_set_max_time_slice_ns) { ENTER; RETV; }
U(mono_gc_start_incremental_collection) { ENTER; RETV; }
U(mono_gc_set_heap_validate_frequency) { ENTER; RETV; }
U(mono_gc_set_heap_verifier_callback) { ENTER; RETV; }
/* Unity stops the world around its liveness walk (below): the GC lock keeps any
 * collection, and so any object move, from starting until it restarts it. */
static int gc_world_stopped;
void sgen_gc_lock (void);
void sgen_gc_unlock (void);
static int lv_env (const char *name) { const char *v = getenv (name); return v && *v && strcmp (v, "0"); }
static void lv_pass_reset (void);
static int lv_on (void);
/* GEXMONO_LV_WATCH=addr,...: the first two words at each address, at every world stop and
 * restart (which objects a liveness pass, or what happens between passes, changes). */
static void
lv_watch (const char *when)
{
	const char *w = getenv ("GEXMONO_LV_WATCH");
	if (!w)
		return;
	char *dup = g_strdup (w), *save = NULL;
	for (char *t = strtok_r (dup, ",", &save); t; t = strtok_r (NULL, ",", &save)) {
		uint64_t *p = (uint64_t *)(uintptr_t)strtoull (t, NULL, 16);
		if (p)
			fprintf (stderr, "[gexmono] lv-watch %s %p: %#llx %#llx %#llx %#llx %#llx\n", when, p, (unsigned long long)p [0],
				(unsigned long long)p [1], (unsigned long long)p [2], (unsigned long long)p [3], (unsigned long long)p [4]);
	}
	g_free (dup);
}
U(mono_unity_stop_gc_world) { ENTER; lv_watch ("stop"); lv_pass_reset (); if (!gc_world_stopped && lv_on () && !lv_env ("GEXMONO_LV_NOLOCK")) { sgen_gc_lock (); gc_world_stopped = 1; } RETV; }
U(mono_unity_start_gc_world) { ENTER; lv_watch ("start"); if (gc_world_stopped) { gc_world_stopped = 0; sgen_gc_unlock (); } RETV; }
U(mono_unity_heap_validation_from_statics) { ENTER; RETV; }

/* ---- Unity's memory profiler: reports nothing ---- */

U(mono_unity_assembly_mempool_chunk_foreach) { ENTER; RETV; }
U(mono_unity_domain_mempool_chunk_foreach) { ENTER; RETV; }
U(mono_unity_root_domain_mempool_chunk_foreach) { ENTER; RETV; }
U(mono_unity_image_set_mempool_chunk_foreach) { ENTER; RETV; }
U(mono_unity_gc_handles_foreach_get_target) { ENTER; RETV; }
U(mono_unity_gc_heap_foreach) { ENTER; RETV; }
U(mono_unity_class_for_each) { ENTER; RETV; }
U(mono_unity_capture_memory_snapshot) { ENTER; RET (0); }
U(mono_unity_free_captured_memory_snapshot) { ENTER; RETV; }
/* ---- liveness: which UnityEngine.Objects managed code still reaches ----
 * Unity's UnloadUnusedAssets keeps an asset only if a managed object reachable
 * from a root (Unity's GC handles, the statics) refers to it: it asks for every
 * object of class `filter` reachable from each root and marks what comes back.
 * Reporting nothing (as the stubs did) let it unload every asset only managed
 * code referred to: Hollow Knight's StartManager lost its loading-spinner
 * prefab at the first scene and its intro never ended. Semantics after Unity's
 * unity-liveness.c: a depth-first walk over reference fields, array elements
 * and value-type fields with references; objects of `filter` are reported to
 * the callback (gpointer *objects, int count, void *userdata) in batches of up
 * to max_count, and the rest at finalize. The walk holds no world stopped: the
 * GC lock, taken by mono_unity_stop_gc_world, keeps a collection from moving
 * objects meanwhile. */
typedef struct {
	MonoClass *filter;
	guint32 max;
	uint64_t cb, data;
	void **arr;
	guint32 n;
	GHashTable *seen;
	GPtrArray *stack;
} Liveness;

static Liveness *
lv_new (MonoClass *filter, guint32 max, uint64_t cb, uint64_t data)
{
	Liveness *s = g_new0 (Liveness, 1);
	s->filter = filter;
	s->max = max ? max : 1024;
	s->cb = cb;
	s->data = data;
	s->arr = g_new0 (void *, s->max);
	s->seen = g_hash_table_new (NULL, NULL);
	s->stack = g_ptr_array_new ();
	return s;
}

static int lv_env (const char *name);
/* The walk is opt-in (GEXMONO_LIVENESS=1) until the Instantiate crash it
 * exposes in Hollow Knight is understood (docs/unity-dotnet.md §12); off, the
 * provider reports nothing, as before. GEXMONO_LV_OFF=1 walks but reports
 * nothing. */
static int
lv_on (void)
{
	static int on = -1;
	if (on < 0)
		on = lv_env ("GEXMONO_LIVENESS");
	return on;
}
/* GEXMONO_LV_TRACE=1: each reported object and each root, as class and Unity instance ID
 * (the int at +8 of the native object m_CachedPtr points to). */
static int
lv_trace (void)
{
	static int t = -1;
	if (t < 0)
		t = getenv ("GEXMONO_LV_TRACE") != NULL;
	return t;
}
static int
lv_iid (MonoObject *o)
{
	/* UnityEngine.Object's first field, m_CachedPtr, right after the header: no metadata
	 * lookups here (the walk runs on Unity job threads Mono does not know). */
	uint8_t *native = *(uint8_t **)((char *)o + MONO_ABI_SIZEOF (MonoObject));
	return (uintptr_t)native > 0x10000 && (uintptr_t)native < ((uintptr_t)1 << 34) ? *(int *)(native + 8) : 0;
}
static void
lv_flush (Liveness *s)
{
	if (lv_trace ())
		for (guint32 i = 0; i < s->n; i++)
			fprintf (stderr, "[gexmono] lv-report %p state %p %s.%s id %d\n", s->arr [i], s, m_class_get_name_space (mono_object_class (s->arr [i])),
				m_class_get_name (mono_object_class (s->arr [i])), lv_iid (s->arr [i]));
	static int off = -1;
	if (off < 0)
		off = lv_env ("GEXMONO_LV_OFF");
	if (s->n && s->cb && !off)
		guest_cb (s->cb, 3, (uintptr_t)s->arr, s->n, s->data, 0, 0);
	s->n = 0;
}

static void
lv_free (Liveness *s)
{
	if (!s)
		return;
	g_hash_table_destroy (s->seen);
	g_ptr_array_free (s->stack, TRUE);
	g_free (s->arr);
	g_free (s);
}

/* Objects already visited in this collection, by any state: Unity's own walk
 * marks objects in their header for the whole pass, so each object reaches the
 * callbacks once however many roots and states reach it (Unity sizes its
 * buffers by the number of objects). Reset when Unity stops the world for the
 * next pass. */
static int lv_env (const char *name);
static GHashTable *lv_pass_seen;
static pthread_mutex_t lv_pass_lock = PTHREAD_MUTEX_INITIALIZER;

static void
lv_pass_reset (void)
{
	pthread_mutex_lock (&lv_pass_lock);
	if (lv_pass_seen)
		g_hash_table_remove_all (lv_pass_seen);
	pthread_mutex_unlock (&lv_pass_lock);
}

static void
lv_push (Liveness *s, MonoObject *o)
{
	if (!o)
		return;
	static int per_state = -1;
	if (per_state < 0)
		per_state = lv_env ("GEXMONO_LV_PERSTATE");
	if (per_state) {
		/* Diagnostic: each state keeps its own visited set, as before the per-pass one. */
		if (g_hash_table_contains (s->seen, o))
			return;
		g_hash_table_add (s->seen, o);
		g_ptr_array_add (s->stack, o);
		return;
	}
	pthread_mutex_lock (&lv_pass_lock);
	if (!lv_pass_seen)
		lv_pass_seen = g_hash_table_new (NULL, NULL);
	gboolean seen = g_hash_table_contains (lv_pass_seen, o);
	if (!seen)
		g_hash_table_add (lv_pass_seen, o);
	pthread_mutex_unlock (&lv_pass_lock);
	if (seen)
		return;
	g_ptr_array_add (s->stack, o);
}

static void lv_fields (Liveness *s, MonoClass *k, char *base, gboolean boxed);

static void
lv_value (Liveness *s, MonoType *t, char *p)
{
	if (m_type_is_byref (t))
		return;
	switch (t->type) {
	case MONO_TYPE_CLASS:
	case MONO_TYPE_OBJECT:
	case MONO_TYPE_SZARRAY:
	case MONO_TYPE_ARRAY:
	case MONO_TYPE_STRING:
		lv_push (s, *(MonoObject **)p);
		break;
	case MONO_TYPE_GENERICINST:
		if (!mono_type_generic_inst_is_valuetype (t)) {
			lv_push (s, *(MonoObject **)p);
			break;
		}
		/* fall through */
	case MONO_TYPE_VALUETYPE: {
		MonoClass *vk = mono_class_from_mono_type_internal (t);
		if (vk && m_class_has_references (vk) && !m_class_is_enumtype (vk))
			lv_fields (s, vk, p, FALSE);
		break;
	}
	default:
		break;
	}
}

/* The instance fields of `k` in an object at `base` (boxed) or a value at
 * `base` (unboxed: field offsets count the object header, which is not there). */
static void
lv_fields (Liveness *s, MonoClass *k, char *base, gboolean boxed)
{
	for (MonoClass *c = k; c && m_class_has_references (c); c = m_class_get_parent (c)) {
		gpointer iter = NULL;
		MonoClassField *f;
		while ((f = mono_class_get_fields_internal (c, &iter))) {
			if (f->type->attrs & (FIELD_ATTRIBUTE_STATIC | FIELD_ATTRIBUTE_LITERAL))
				continue;
			int off = m_field_get_offset (f) - (boxed ? 0 : (int)MONO_ABI_SIZEOF (MonoObject));
			lv_value (s, f->type, base + off);
		}
		if (!boxed)
			break;
	}
}

static void
lv_object_walk (Liveness *s, MonoObject *o, gboolean report)
{
	MonoClass *k = mono_object_class (o);
	if (report && s->filter && mono_class_is_subclass_of_internal (k, s->filter, FALSE)) {
		s->arr [s->n++] = o;
		if (s->n >= s->max)
			lv_flush (s);
	}
	if (m_class_get_rank (k)) {
		MonoArray *a = (MonoArray *)o;
		MonoClass *ek = m_class_get_element_class (k);
		uintptr_t len = mono_array_length_internal (a);
		if (!m_class_is_valuetype (ek)) {
			for (uintptr_t i = 0; i < len; i++)
				lv_push (s, mono_array_get_internal (a, MonoObject *, i));
		} else if (m_class_has_references (ek)) {
			int es = mono_array_element_size (k);
			for (uintptr_t i = 0; i < len; i++)
				lv_fields (s, ek, mono_array_addr_with_size_internal (a, es, i), FALSE);
		}
		return;
	}
	if (k == mono_defaults.string_class)
		return;
	lv_fields (s, k, (char *)o, TRUE);
}

static void
lv_drain (Liveness *s)
{
	while (s->stack->len) {
		MonoObject *o = (MonoObject *)g_ptr_array_index (s->stack, s->stack->len - 1);
		g_ptr_array_set_size (s->stack, s->stack->len - 1);
		lv_object_walk (s, o, TRUE);
	}
}

static void
lv_class_statics (gpointer value, gpointer user_data)
{
	MonoClass *k = (MonoClass *)value;
	Liveness *s = (Liveness *)user_data;
	if (!k || mono_class_is_gtd (k))
		return;
	MonoVTable *vt = mono_class_try_get_vtable (k);
	if (!vt || !m_class_has_static_refs (k))
		return;
	char *data = (char *)mono_vtable_get_static_field_data (vt);
	if (!data)
		return;
	gpointer iter = NULL;
	MonoClassField *f;
	while ((f = mono_class_get_fields_internal (k, &iter))) {
		if (!(f->type->attrs & FIELD_ATTRIBUTE_STATIC) || (f->type->attrs & FIELD_ATTRIBUTE_LITERAL))
			continue;
		if (mono_field_is_deleted (f) || mono_class_field_is_special_static (f))
			continue;
		lv_value (s, f->type, data + m_field_get_offset (f));
	}
}


void sgen_gc_lock (void);
void sgen_gc_unlock (void);
static int lv_world_stopped;

/* allocate_struct (filter, max_count, callback, userdata, reallocate): Unity 2021+. */
U(mono_unity_liveness_allocate_struct) { ENTER; RET (lv_new ((MonoClass *)P (0), (guint32)A (1), A (2), A (3))); }
U(mono_unity_liveness_free_struct) { ENTER; lv_free ((Liveness *)P (0)); RETV; }
U(mono_unity_liveness_finalize) { ENTER; if (P (0)) lv_flush ((Liveness *)P (0)); RETV; }
/* calculation_begin (filter, max_count, callback, userdata, world_start, world_stop): the older form. */
U(mono_unity_liveness_calculation_begin) { ENTER; lv_pass_reset (); RET (lv_new ((MonoClass *)P (0), (guint32)A (1), A (2), A (3))); }
U(mono_unity_liveness_calculation_end) { ENTER; if (P (0)) lv_flush ((Liveness *)P (0)); lv_free ((Liveness *)P (0)); RETV; }
U(mono_unity_liveness_calculation_from_root)
{
	ENTER;
	Liveness *s = (Liveness *)P (1);
	if (s && lv_on () && P (0)) {
		/* As Unity's from_root: the root's references are walked, the root itself is
		 * neither marked nor reported (it is reported if another path reaches it). The
		 * roots are the managed wrappers Unity holds GC handles to; reporting them kept
		 * every component alive whatever its GameObject's fate (a MonoBehaviour outlived
		 * its unloaded GameObject and Object.name followed the dangling pointer). */
		if (lv_trace ())
			fprintf (stderr, "[gexmono] lv-root %p state %p %s.%s id %d\n", P (0), s, m_class_get_name_space (mono_object_class (P (0))),
				m_class_get_name (mono_object_class (P (0))), lv_iid (P (0)));
		lv_object_walk (s, (MonoObject *)P (0), FALSE);
		lv_drain (s);
	}
	RETV;
}
U(mono_unity_liveness_calculation_from_statics)
{
	ENTER;
	Liveness *s = (Liveness *)P (0);
	if (s && lv_on ()) {
		/* As Unity's from_statics: every class with a vtable (generic instances
		 * included), corlib's skipped. */
		MonoMemoryManager *mm = mono_mem_manager_get_ambient ();
		mono_mem_manager_lock (mm);
		GPtrArray *vts = g_ptr_array_new ();
		for (guint i = 0; mm->class_vtable_array && i < mm->class_vtable_array->len; i++)
			g_ptr_array_add (vts, g_ptr_array_index (mm->class_vtable_array, i));
		mono_mem_manager_unlock (mm);
		for (guint i = 0; i < vts->len; i++) {
			MonoVTable *vt = (MonoVTable *)g_ptr_array_index (vts, i);
			MonoClass *k = vt ? vt->klass : NULL;
			if (k && m_class_get_image (k) != mono_defaults.corlib)
				lv_class_statics (k, s);
		}
		g_ptr_array_free (vts, TRUE);
		lv_drain (s);
	}
	RETV;
}
U(mono_unity_liveness_start_gc_world) { ENTER; if (lv_world_stopped) { lv_world_stopped = 0; sgen_gc_unlock (); } RETV; }
U(mono_unity_liveness_stop_gc_world) { ENTER; lv_pass_reset (); if (!lv_world_stopped && lv_on ()) { sgen_gc_lock (); lv_world_stopped = 1; } RETV; }
U(mono_unity_managed_callstack) { ENTER; RET (0); }
U(mono_unity_lock_dynamic_function_access_tables64) { ENTER; RETV; }
U(mono_unity_unlock_dynamic_function_access_tables64) { ENTER; RETV; }
U(mono_profiler_install) { ENTER; RETV; }
U(mono_profiler_install_gc) { ENTER; RETV; }
U(mono_profiler_set_events) { ENTER; RETV; }
U(mono_profiler_load) { ENTER; RETV; }
U(mono_profiler_create) { ENTER; static char handle [64]; RET (handle); }
U(mono_profiler_enable_coverage) { ENTER; RET (0); }
U(mono_profiler_set_coverage_filter_callback) { ENTER; RETV; }
U(mono_profiler_get_coverage_data) { ENTER; RET (0); }
U(mono_profiler_get_all_coverage_data) { ENTER; RETV; }
U(mono_profiler_reset_coverage) { ENTER; RETV; }
U(mono_profiler_reset_all_coverage) { ENTER; RETV; }

/* ---- guest callbacks ---- */

/* mono_assembly_foreach (func, user_data): func (assembly, user_data). */
typedef struct { uint64_t fn, data; } GuestCb;
static void
assembly_cb (gpointer assembly, gpointer d)
{
	GuestCb *c = d;
	guest_cb (c->fn, 2, (uintptr_t)assembly, c->data, 0, 0, 0);
}
U(mono_assembly_foreach) { ENTER; GuestCb c = { A (0), A (1) }; mono_assembly_foreach (assembly_cb, &c); RETV; }

/* mono_stack_walk[_no_il] (func, user_data): func (method, native_offset,
 * il_offset, managed, data) returns TRUE to stop. */
static gboolean
stack_cb (MonoMethod *m, gint32 native, gint32 il, gboolean managed, gpointer d)
{
	GuestCb *c = d;
	return (int)guest_cb (c->fn, 5, (uintptr_t)m, (uint32_t)native, (uint32_t)il, managed, c->data) != 0;
}
U(mono_stack_walk) { ENTER; GuestCb c = { A (0), A (1) }; mono_stack_walk (stack_cb, &c); RETV; }
U(mono_stack_walk_no_il) { ENTER; GuestCb c = { A (0), A (1) }; mono_stack_walk_no_il (stack_cb, &c); RETV; }

/* mono_trace_set_log_handler (callback, user_data): callback (domain, level,
 * message, fatal, user_data). */
static GuestCb log_cb;
static void
log_handler (const char *domain, const char *level, const char *message, mono_bool fatal, void *d)
{
	guest_cb (log_cb.fn, 5, (uintptr_t)domain, (uintptr_t)level, (uintptr_t)message, fatal, log_cb.data);
}
U(mono_trace_set_log_handler) { ENTER; log_cb.fn = A (0); log_cb.data = A (1); mono_trace_set_log_handler (log_handler, NULL); RETV; }

/* ---- where assemblies and native plugins come from ----
 * Unity's own BCL (mscorlib and the rest of its Mono profile) does not run
 * on upstream Mono: .NET's CoreLib and Framework facades (GEXMONO_ASSEMBLIES)
 * stand in for it. Unity's game assemblies come from where it says. */

char *gexmono_unity_managed_dir;

static void
set_assemblies_path (const char *unity_path)
{
	const char *ours = getenv ("GEXMONO_ASSEMBLIES");
	GString *s = g_string_new (ours ? ours : "");
	if (unity_path && *unity_path) {
		if (s->len)
			g_string_append_c (s, ':');
		g_string_append (s, unity_path);
		g_free (gexmono_unity_managed_dir);
		gexmono_unity_managed_dir = g_strdup (unity_path);
		char *colon = strchr (gexmono_unity_managed_dir, ':');
		if (colon)
			*colon = 0;
	}
	mono_set_assemblies_path (s->str);
	g_string_free (s, TRUE);
}

/* Unity 6 maps DllImport names (mono_dllmap_insert), which .NET's Mono does not have: noted, and left
 * to the find-plugin callback and dl fallback, which see the original name. */
U(mono_dllmap_insert)
{
	ENTER;
	fprintf (stderr, "[gexmono] dllmap %s%s%s -> %s%s%s (ignored)\n", P (1) ? (const char *)P (1) : "?", P (2) ? ":" : "",
		P (2) ? (const char *)P (2) : "", P (3) ? (const char *)P (3) : "?", P (4) ? ":" : "", P (4) ? (const char *)P (4) : "");
	RETV;
}
U(mono_set_dirs) { ENTER; set_assemblies_path (P (0)); RETV; }
U(mono_set_assemblies_path) { ENTER; set_assemblies_path (P (0)); RETV; }
U(mono_set_assemblies_path_null_separated)
{
	ENTER;
	GString *s = g_string_new ("");
	for (const char *p = P (0); p && *p; p += strlen (p) + 1) {
		if (s->len)
			g_string_append_c (s, ':');
		g_string_append (s, p);
	}
	set_assemblies_path (s->str);
	g_string_free (s, TRUE);
	RETV;
}

/* Unity's find-plugin callback, const char *(*)(const char *name): the path
 * of a native plugin; dl_load (gexmono-provider.c) asks it first. */
uint64_t gexmono_find_plugin;
U(mono_set_find_plugin_callback) { ENTER; gexmono_find_plugin = A (0); RETV; }

/* Unity's own dl fallback (load, symbol, close, user_data), kept for
 * dl_load/dl_symbol after the guest's dlopen (gexmono-provider.c). */
uint64_t gexmono_dl_fallback [4];
U(mono_dl_fallback_register)
{
	ENTER;
	for (int i = 0; i < 4; i++)
		gexmono_dl_fallback [i] = A (i);
	RET (calloc (1, 16));
}

#define L(n) gexp_##n,
__attribute__((used)) gexp_body_t const gexmono_unity_adapters [] = {
	L(mono_unity_object_header_size) L(mono_unity_array_object_header_size)
	L(mono_unity_offset_of_array_length_in_array_object_header) L(mono_unity_offset_of_array_bounds_in_array_object_header)
	L(mono_unity_allocation_granularity) L(mono_class_get_userdata) L(mono_class_set_userdata)
	L(mono_class_get_userdata_offset) L(mono_class_is_blittable) L(mono_class_is_generic) L(mono_class_is_inflated)
	L(mono_unity_class_is_interface) L(mono_unity_class_is_abstract) L(mono_unity_class_is_inited)
	L(mono_unity_class_has_failure) L(mono_unity_class_get_data_size) L(mono_unity_class_is_open_constructed_type)
	L(mono_unity_class_try_get_vtable) L(mono_unity_class_field_is_literal) L(mono_unity_class_get_generic_type_definition)
	L(mono_unity_class_get) L(mono_unity_class_get_checked) L(mono_unity_field_from_token_checked)
	L(mono_unity_get_method_checked) L(mono_unity_type_is_pointer_type) L(mono_unity_type_is_static) L(mono_type_get_attrs)
	L(mono_unity_vtable_get_static_field_data) L(mono_method_signature_checked) L(mono_method_signature_checked_slow)
	L(mono_unity_type_get_name_full_chunked) L(mono_custom_attrs_get_attrs) L(mono_custom_attrs_construct_by_type)
	L(mono_unity_custom_attrs_construct) L(mono_unity_array_new_2d) L(mono_unity_array_new_3d)
	L(mono_unity_string_empty_wrapper) L(mono_unity_error_convert_to_exception)
	L(mono_unity_loader_get_last_error_and_error_prepare_exception) L(mono_unity_g_free) L(mono_assembly_name_parse)
	L(mono_unity_set_data_dir) L(mono_unity_set_embeddinghostname) L(mono_unity_domain_set_config)
	L(mono_unity_runtime_set_main_args) L(mono_unity_install_memory_callbacks) L(mono_unity_install_unitytls_interface)
	L(mono_unity_set_vprintf_func) L(mono_unity_jit_cleanup) L(mono_set_defaults) L(mono_parse_default_optimizations)
	L(mono_verifier_set_mode) L(mono_set_ignore_version_and_key_when_finding_assemblies_already_loaded)
	L(mono_runtime_unhandled_exception_policy_set) L(mono_threads_set_shutting_down)
	L(mono_thread_suspend_all_other_threads) L(mono_thread_pool_cleanup) L(mono_thread_push_appdomain_ref)
	L(mono_thread_pop_appdomain_ref) L(mono_thread_has_sufficient_execution_stack) L(mono_gchandle_is_in_domain)
	L(mono_gchandle_is_in_domain_v2) L(mono_gchandle_new_v2) L(mono_gchandle_new_weakref_v2) L(mono_gchandle_get_target_v2) L(mono_gchandle_free_v2) L(mono_gchandle_new) L(mono_gchandle_new_weakref) L(mono_gchandle_get_target) L(mono_gchandle_free) L(mono_pmip) L(mono_debug_lookup_source_location_by_il)
	L(mono_unity_gc_set_mode) L(mono_unity_gc_disable) L(mono_unity_gc_enable) L(mono_unity_gc_is_disabled)
	L(mono_gc_is_incremental) L(mono_gc_set_incremental) L(mono_gc_collect_a_little) L(mono_gc_get_max_time_slice_ns)
	L(mono_gc_set_max_time_slice_ns) L(mono_gc_start_incremental_collection) L(mono_gc_set_heap_validate_frequency)
	L(mono_gc_set_heap_verifier_callback) L(mono_unity_stop_gc_world) L(mono_unity_start_gc_world)
	L(mono_unity_heap_validation_from_statics) L(mono_unity_assembly_mempool_chunk_foreach)
	L(mono_unity_domain_mempool_chunk_foreach) L(mono_unity_root_domain_mempool_chunk_foreach)
	L(mono_unity_image_set_mempool_chunk_foreach) L(mono_unity_gc_handles_foreach_get_target) L(mono_unity_gc_heap_foreach)
	L(mono_unity_class_for_each) L(mono_unity_capture_memory_snapshot) L(mono_unity_free_captured_memory_snapshot)
	L(mono_unity_liveness_allocate_struct) L(mono_unity_liveness_free_struct) L(mono_unity_liveness_finalize)
	L(mono_unity_liveness_calculation_begin) L(mono_unity_liveness_calculation_end)
	L(mono_unity_liveness_calculation_from_root) L(mono_unity_liveness_calculation_from_statics)
	L(mono_unity_liveness_start_gc_world) L(mono_unity_liveness_stop_gc_world) L(mono_unity_managed_callstack)
	L(mono_unity_lock_dynamic_function_access_tables64) L(mono_unity_unlock_dynamic_function_access_tables64)
	L(mono_profiler_install) L(mono_profiler_install_gc) L(mono_profiler_set_events) L(mono_profiler_load)
	L(mono_profiler_create) L(mono_profiler_enable_coverage) L(mono_profiler_set_coverage_filter_callback)
	L(mono_profiler_get_coverage_data) L(mono_profiler_get_all_coverage_data) L(mono_profiler_reset_coverage)
	L(mono_profiler_reset_all_coverage) L(mono_assembly_foreach) L(mono_stack_walk) L(mono_stack_walk_no_il)
	L(mono_trace_set_log_handler) L(mono_dllmap_insert) L(mono_set_dirs) L(mono_set_assemblies_path) L(mono_set_assemblies_path_null_separated)
	L(mono_set_find_plugin_callback) L(mono_dl_fallback_register)
	L(mono_class_get_field_from_name) L(mono_class_is_subclass_of) L(mono_class_get_methods) L(mono_array_new) L(mono_gc_wbarrier_set_field) L(mono_custom_attrs_from_field) L(unity_mono_method_is_generic) L(unity_mono_method_is_inflated) L(unity_mono_reflection_method_get_method)
	0
};
