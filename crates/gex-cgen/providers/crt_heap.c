/* PROVIDES msvcr120.dll i386 stateful native nowine
 *
 * The CRT heap as native wasm (docs/merge-plan.md §9.9.2 step 4): dlmalloc
 * (2.8.6, as pewasm's dlmalloc32.inc) in place of msvcr120's heap over
 * HeapAlloc(_crtheap). The whole family at once, because two allocators on
 * one heap corrupt it: every export that reaches _crtheap itself is here
 * (malloc, free, realloc, calloc, _msize, _expand, _recalloc, _calloc_crt);
 * the rest (operator new and delete, _malloc_crt, _realloc_crt, _strdup,
 * the _aligned_* family) call those exports, which the library's unit
 * routes here too.
 *
 * The heap grows over guest pages from the NT allocator, top down
 * (gex.provider_alloc_top, so the DLLs the program maps later land where
 * their units expect), and every block is a guest pointer. A pointer the
 * heap did not hand out (one from HeapAlloc(_get_heap_handle()), which no
 * known caller does) is reported once; free leaves it, and realloc,
 * _msize and _expand stop the run rather than guess its size.
 *
 * One lock (a futex on wasm atomics) for the heap, as Windows' process heap
 * and pewasm's mspace have. */

__attribute__((import_module("gex"), import_name("provider_alloc_top"))) int gex_provider_alloc(int size);
__attribute__((import_module("gex"), import_name("provider_log"))) void gex_provider_log(const char *p, int n);

typedef __SIZE_TYPE__ size_t;
typedef __PTRDIFF_TYPE__ ptrdiff_t;

/* ---- the lock ---- */
/* A word: 0 free, 1 held, 2 held with waiters. */
static void heap_lock_slow(int *w) {
    for (int spin = 0; spin < 64; ++spin) {
        int z = 0;
        if (__atomic_load_n(w, __ATOMIC_RELAXED) == 0 && __atomic_compare_exchange_n(w, &z, 1, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
            return;
        }
    }
    while (__atomic_exchange_n(w, 2, __ATOMIC_ACQUIRE) != 0) {
        (void)__builtin_wasm_memory_atomic_wait32(w, 2, -1);
    }
}
static inline int heap_acquire(int *w) {
    int z = 0;
    if (!__atomic_compare_exchange_n(w, &z, 1, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
        heap_lock_slow(w);
    }
    return 0;
}
static inline void heap_release(int *w) {
    if (__atomic_exchange_n(w, 0, __ATOMIC_RELEASE) == 2) {
        (void)__builtin_wasm_memory_atomic_notify(w, 1);
    }
}
static inline int heap_try(int *w) {
    int z = 0;
    return __atomic_compare_exchange_n(w, &z, 1, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED);
}

/* ---- the heap's own pages: 64 KiB granules, a bit each ---- */
static uint32_t owned[(1u << 16) / 32];
static void *heap_pages(size_t n) {
    int a = gex_provider_alloc((int)n);
    if (!a) {
        return (void *)~(size_t)0;
    }
    for (uint32_t g = (uint32_t)a >> 16, e = ((uint32_t)a + (uint32_t)n + 0xffffu) >> 16; g < e; ++g) {
        owned[g >> 5] |= 1u << (g & 31);
    }
    return (void *)(size_t)(uint32_t)a;
}
static inline int heap_owns(uint32_t p) { return (owned[p >> 21] >> ((p >> 16) & 31)) & 1; }

/* ---- dlmalloc, freestanding ---- */
#define USE_DL_PREFIX 1
#define DLMALLOC_EXPORT static
#define LACKS_UNISTD_H
#define LACKS_FCNTL_H
#define LACKS_SYS_PARAM_H
#define LACKS_SYS_MMAN_H
#define LACKS_STRINGS_H
#define LACKS_STRING_H
#define LACKS_SYS_TYPES_H
#define LACKS_ERRNO_H
#define LACKS_STDLIB_H
#define LACKS_SCHED_H
#define LACKS_TIME_H
#define NO_MALLOC_STATS 1
#define NO_MALLINFO 1
#define ABORT __builtin_trap()
#define MALLOC_FAILURE_ACTION
#define HAVE_MORECORE 0
#define HAVE_MMAP 1
#define MMAP(s) heap_pages(s)
#define DIRECT_MMAP(s) heap_pages(s)
#define MUNMAP(a, s) (-1)
#define MALLOC_ALIGNMENT 16
/* Every chunk inside a segment (none mapped alone: the NT allocator's pages
 * are not given back). */
#define DEFAULT_MMAP_THRESHOLD ((size_t)-1)
#define DEFAULT_GRANULARITY ((size_t)4 << 20)
#define USE_LOCKS 2
#define MLOCK_T int
#define INITIAL_LOCK(l) (*(l) = 0)
#define DESTROY_LOCK(l) (0)
#define ACQUIRE_LOCK(l) heap_acquire(l)
#define RELEASE_LOCK(l) heap_release(l)
#define TRY_LOCK(l) heap_try(l)
static MLOCK_T malloc_global_mutex = 0;
#define memset __builtin_memset
#define memcpy __builtin_memcpy
#define EINVAL 22
#define ENOMEM 12
#include "dlmalloc.inc"
#undef memset
#undef memcpy

/* ---- the exports (all cdecl) ---- */
static void foreign(const char *what, uint32_t p) {
    static int told;
    if (!told) {
        told = 1;
        char m[] = "gex crt_heap: XXXXXXXX: a pointer this heap did not hand out (0x00000000)\n";
        for (int i = 0; i < 8 && what[i]; ++i) {
            m[14 + i] = what[i];
        }
        for (int i = 0; i < 8; ++i) {
            m[66 + i] = "0123456789abcdef"[(p >> (28 - 4 * i)) & 15];
        }
        gex_provider_log(m, sizeof m - 1);
    }
}

static GEX_PROVIDER(p_malloc) {
    (void)depth, (void)need;
    uint32_t n = gp_arg(ctx, 0);
    void *p = n <= 0xffffffe0u ? dlmalloc(n ? n : 1) : 0;
    return gp_return_eax(ctx, ret, 0, (uint32_t)(size_t)p);
}
static GEX_PROVIDER(p_calloc) {
    (void)depth, (void)need;
    uint64_t n = (uint64_t)gp_arg(ctx, 0) * gp_arg(ctx, 1);
    void *p = n <= 0xffffffe0u ? dlcalloc(1, n ? (size_t)n : 1) : 0;
    return gp_return_eax(ctx, ret, 0, (uint32_t)(size_t)p);
}
static GEX_PROVIDER(p_free) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0);
    if (p) {
        if (heap_owns(p)) {
            dlfree((void *)(size_t)p);
        } else {
            foreign("free", p);
        }
    }
    return gp_return(ctx, ret, 0);
}
static GEX_PROVIDER(p_realloc) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0), n = gp_arg(ctx, 1);
    if (p && !heap_owns(p)) {
        foreign("realloc", p);
        __builtin_trap();
    }
    uint32_t r;
    if (!p) {
        r = (uint32_t)(size_t)dlmalloc(n ? n : 1);
    } else if (!n) {
        dlfree((void *)(size_t)p);
        r = 0;
    } else {
        r = n <= 0xffffffe0u ? (uint32_t)(size_t)dlrealloc((void *)(size_t)p, n) : 0;
    }
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(p_recalloc) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0);
    uint64_t n = (uint64_t)gp_arg(ctx, 1) * gp_arg(ctx, 2);
    if (p && !heap_owns(p)) {
        foreign("_recallo", p);
        __builtin_trap();
    }
    uint32_t r = 0;
    if (n <= 0xffffffe0u) {
        size_t old = p ? dlmalloc_usable_size((void *)(size_t)p) : 0;
        void *q = p ? dlrealloc((void *)(size_t)p, n ? (size_t)n : 1) : dlmalloc(n ? (size_t)n : 1);
        if (q) {
            size_t now = dlmalloc_usable_size(q);
            if (now > old) {
                __builtin_memset((char *)q + old, 0, now - old);
            }
            r = (uint32_t)(size_t)q;
        }
    }
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(p_msize) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0);
    if (!heap_owns(p)) {
        foreign("_msize", p);
        __builtin_trap();
    }
    return gp_return_eax(ctx, ret, 0, (uint32_t)dlmalloc_usable_size((void *)(size_t)p));
}
static GEX_PROVIDER(p_expand) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0), n = gp_arg(ctx, 1);
    if (!heap_owns(p)) {
        foreign("_expand", p);
        __builtin_trap();
    }
    void *q = n <= 0xffffffe0u ? dlrealloc_in_place((void *)(size_t)p, n ? n : 1) : 0;
    return gp_return_eax(ctx, ret, 0, (uint32_t)(size_t)q);
}

GEX_PROVIDE(p_malloc, "malloc")
GEX_PROVIDE(p_calloc, "calloc")
GEX_PROVIDE(p_calloc, "_calloc_crt")
GEX_PROVIDE(p_free, "free")
GEX_PROVIDE(p_realloc, "realloc")
GEX_PROVIDE(p_recalloc, "_recalloc")
GEX_PROVIDE(p_msize, "_msize")
GEX_PROVIDE(p_expand, "_expand")
