/* PROVIDES msvcr120.dll i386 stateful wineonly
 *
 * crt_heap.c's dlmalloc heap for Wine's own build of msvcr120 (the one
 * 32-bit Wine runs), whose heap code crt_heap.c cannot route: Wine's operator new and
 * delete, calloc, realloc, _expand, _recalloc and _aligned_offset_realloc
 * reach the process heap through static helpers (msvcrt_heap_alloc,
 * msvcrt_heap_realloc) or HeapFree/HeapSize directly, not through the
 * exports. So every export of dlls/msvcrt/heap.c that reaches the heap is
 * here; the rest (_aligned_malloc, _aligned_offset_malloc, _aligned_free,
 * _aligned_msize, _aligned_realloc, _strdup and the like) call these exports,
 * which the library's unit routes here. A program that allocates through
 * operator new and delete every frame otherwise spends a measurable part
 * of its main thread in Wine's heap.
 *
 * Wine's semantics, as dlls/msvcrt/heap.c has them over HeapAlloc:
 * - _msize is the size asked for (HeapSize), not the block's: a block keeps
 *   its slack (usable size minus size asked, 1..255) in its last byte, so
 *   every block is asked of dlmalloc one byte longer.
 * - realloc(p, 0) frees and returns NULL; realloc(NULL, n) is malloc(n).
 * - calloc fails on overflow of count * size; _recalloc does not check it.
 * - _recalloc zeroes from the old _msize to the new size.
 * - _expand resizes in place or returns NULL; _expand(NULL, n) is NULL.
 * - _msize(NULL) is -1 (Wine's faults in HeapSize).
 * - operator new and malloc never call the new handler or set errno here:
 *   an allocation failure in a 32-bit process is reported and traps.
 * A pointer this heap did not hand out (HeapAlloc(GetProcessHeap()) from
 * another library, or a block from before the unit bound these) is
 * reported once: free leaves it, the rest trap rather than guess its size.
 *
 * The heap's pages, lock and dlmalloc configuration are crt_heap.c's. */

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
        char m[] = "gex crt_heap_wine: XXXXXXXX: a pointer this heap did not hand out (0x00000000)\n";
        for (int i = 0; i < 8 && what[i]; ++i) {
            m[19 + i] = what[i];
        }
        for (int i = 0; i < 8; ++i) {
            m[71 + i] = "0123456789abcdef"[(p >> (28 - 4 * i)) & 15];
        }
        gex_provider_log(m, sizeof m - 1);
    }
}
static void out_of_memory(uint32_t n) {
    char m[] = "gex crt_heap_wine: out of memory for 0x00000000 bytes\n";
    for (int i = 0; i < 8; ++i) {
        m[40 + i] = "0123456789abcdef"[(n >> (28 - 4 * i)) & 15];
    }
    gex_provider_log(m, sizeof m - 1);
    __builtin_trap();
}

/* A block of `n` bytes: one more asked of dlmalloc, the slack kept in its
 * last byte. */
static inline void mark(void *q, uint32_t n) {
    size_t u = dlmalloc_usable_size(q);
    uint32_t slack = (uint32_t)u - n;
    ((unsigned char *)q)[u - 1] = (unsigned char)(slack > 255 ? 255 : slack);
}
/* What _msize answers (HeapSize: the size asked for). */
static inline uint32_t size_of(uint32_t p) {
    size_t u = dlmalloc_usable_size((void *)(size_t)p);
    return (uint32_t)u - ((unsigned char *)(size_t)p)[u - 1];
}
static uint32_t h_alloc(uint32_t n, int zero) {
    if (n > 0xffffffe0u) {
        return 0;
    }
    void *q = zero ? dlcalloc(1, (size_t)n + 1) : dlmalloc((size_t)n + 1);
    if (!q) {
        return 0;
    }
    mark(q, n);
    return (uint32_t)(size_t)q;
}
static void h_free(const char *what, uint32_t p) {
    if (!p) {
        return;
    }
    if (heap_owns(p)) {
        dlfree((void *)(size_t)p);
    } else {
        foreign(what, p);
    }
}
static uint32_t owned_or_trap(const char *what, uint32_t p) {
    if (!heap_owns(p)) {
        foreign(what, p);
        __builtin_trap();
    }
    return p;
}
/* msvcrt_heap_realloc: HeapReAlloc(heap, 0, p, n), p non-null. */
static uint32_t h_realloc(const char *what, uint32_t p, uint32_t n) {
    owned_or_trap(what, p);
    if (n > 0xffffffe0u) {
        return 0;
    }
    void *q = dlrealloc((void *)(size_t)p, (size_t)n + 1);
    if (!q) {
        return 0;
    }
    mark(q, n);
    return (uint32_t)(size_t)q;
}

static GEX_PROVIDER(w_malloc) {
    (void)depth, (void)need;
    uint32_t n = gp_arg(ctx, 0);
    uint32_t r = h_alloc(n, 0);
    if (!r) {
        out_of_memory(n);
    }
    return gp_return_eax(ctx, ret, 0, r);
}
/* operator new, new[] (and their _dbg forms, whose extra arguments are
 * ignored as Wine's are). */
static GEX_PROVIDER(w_new) {
    (void)depth, (void)need;
    uint32_t n = gp_arg(ctx, 0);
    uint32_t r = h_alloc(n, 0);
    if (!r) {
        out_of_memory(n);
    }
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(w_calloc) {
    (void)depth, (void)need;
    uint32_t count = gp_arg(ctx, 0), size = gp_arg(ctx, 1);
    uint32_t bytes = count * size;
    if (size && bytes / size != count) {
        return gp_return_eax(ctx, ret, 0, 0);
    }
    uint32_t r = h_alloc(bytes, 1);
    if (!r) {
        out_of_memory(bytes);
    }
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(w_free) {
    (void)depth, (void)need;
    h_free("free", gp_arg(ctx, 0));
    return gp_return(ctx, ret, 0);
}
static GEX_PROVIDER(w_delete) {
    (void)depth, (void)need;
    h_free("delete", gp_arg(ctx, 0));
    return gp_return(ctx, ret, 0);
}
static GEX_PROVIDER(w_realloc) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0), n = gp_arg(ctx, 1), r;
    if (!p) {
        r = h_alloc(n, 0);
        if (!r) {
            out_of_memory(n);
        }
    } else if (n) {
        r = h_realloc("realloc", p, n);
    } else {
        h_free("realloc", p);
        r = 0;
    }
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(w_recalloc) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0), num = gp_arg(ctx, 1), size = gp_arg(ctx, 2), r;
    if (!p) {
        /* calloc(num, size) */
        uint32_t bytes = num * size;
        r = (size && bytes / size != num) ? 0 : h_alloc(bytes, 1);
        return gp_return_eax(ctx, ret, 0, r);
    }
    size = num * size;
    uint32_t old = size_of(owned_or_trap("_recallo", p));
    /* realloc(p, size), Wine's inlined: size 0 frees and gives NULL. */
    if (size) {
        r = h_realloc("_recallo", p, size);
    } else {
        h_free("_recallo", p);
        r = 0;
    }
    if (r && size > old) {
        __builtin_memset((char *)(size_t)r + old, 0, size - old);
    }
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(w_msize) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0);
    if (!p) {
        return gp_return_eax(ctx, ret, 0, 0xffffffffu);
    }
    return gp_return_eax(ctx, ret, 0, size_of(owned_or_trap("_msize", p)));
}
static GEX_PROVIDER(w_expand) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 0), n = gp_arg(ctx, 1), r = 0;
    if (p && n <= 0xffffffe0u) {
        owned_or_trap("_expand", p);
        void *q = dlrealloc_in_place((void *)(size_t)p, (size_t)n + 1);
        if (q) {
            mark(q, n);
            r = (uint32_t)(size_t)q;
        }
    }
    return gp_return_eax(ctx, ret, 0, r);
}
/* _aligned_offset_realloc, line for line as Wine's (dlls/msvcrt/heap.c),
 * whose realloc is msvcrt_heap_realloc inlined. The block's real start is
 * the word below the pointer; the allocation is size + alignment + 4. */
#define SAVED(x) ((uint32_t)(((x) - 4u) & ~3u))
#define ALIGNED(ptr, alignment, offset) ((((ptr) + (alignment) + 4u + (offset)) & ~((alignment) - 1u)) - (offset))
static uint32_t aligned_offset_malloc(uint32_t size, uint32_t alignment, uint32_t offset) {
    if (alignment & (alignment - 1u)) {
        return 0;
    }
    if (offset && offset >= size) {
        return 0;
    }
    if (alignment < 4u) {
        alignment = 4u;
    }
    uint32_t temp = h_alloc(size + alignment + 4u, 0);
    if (!temp) {
        return 0;
    }
    uint32_t memblock = ALIGNED(temp, alignment, offset);
    GW(SAVED(memblock)) = temp;
    return memblock;
}
static uint32_t aligned_offset_realloc(uint32_t memblock, uint32_t size, uint32_t alignment, uint32_t offset) {
    if (!memblock) {
        return aligned_offset_malloc(size, alignment, offset);
    }
    if (alignment & (alignment - 1u)) {
        return 0;
    }
    if (offset >= size) {
        return 0;
    }
    if (size == 0) {
        h_free("_aligned", GW(SAVED(memblock)));
        return 0;
    }
    if (alignment < 4u) {
        alignment = 4u;
    }
    uint32_t alloc_ptr = GW(SAVED(memblock));
    uint32_t old_padding = memblock - alloc_ptr;
    uint32_t old_size = size_of(owned_or_trap("_aligned", alloc_ptr));
    if (old_size < old_padding) {
        return 0;
    }
    old_size -= old_padding;
    if (alignment + 4u < old_padding) {
        __builtin_memmove((void *)(size_t)alloc_ptr, (void *)(size_t)memblock, old_size);
        old_padding = 0;
    }
    uint32_t temp = h_realloc("_aligned", alloc_ptr, size + alignment + 4u);
    if (!temp) {
        if (!old_padding) {
            __builtin_memmove((void *)(size_t)memblock, (void *)(size_t)alloc_ptr, old_size);
            GW(SAVED(memblock)) = alloc_ptr;
        }
        return 0;
    }
    memblock = ALIGNED(temp, alignment, offset);
    uint32_t new_padding = memblock - temp;
    if (new_padding != old_padding) {
        __builtin_memmove((void *)(size_t)memblock, (void *)(size_t)(temp + old_padding), old_size < size ? old_size : size);
    }
    GW(SAVED(memblock)) = temp;
    return memblock;
}
static GEX_PROVIDER(w_aligned_offset_realloc) {
    (void)depth, (void)need;
    uint32_t r = aligned_offset_realloc(gp_arg(ctx, 0), gp_arg(ctx, 1), gp_arg(ctx, 2), gp_arg(ctx, 3));
    return gp_return_eax(ctx, ret, 0, r);
}
static GEX_PROVIDER(w_aligned_realloc) {
    (void)depth, (void)need;
    uint32_t r = aligned_offset_realloc(gp_arg(ctx, 0), gp_arg(ctx, 1), gp_arg(ctx, 2), 0);
    return gp_return_eax(ctx, ret, 0, r);
}

GEX_GROUP("malloc,calloc,free,realloc,_recalloc,_msize,_expand,??2@YAPAXI@Z,??_U@YAPAXI@Z,??3@YAXPAX@Z,??_V@YAXPAX@Z,_aligned_offset_realloc,_aligned_realloc")
GEX_PROVIDE(w_malloc, "malloc")
GEX_PROVIDE(w_malloc, "_malloc_crt")
GEX_PROVIDE(w_calloc, "calloc")
GEX_PROVIDE(w_calloc, "_calloc_crt")
GEX_PROVIDE(w_free, "free")
GEX_PROVIDE(w_realloc, "realloc")
GEX_PROVIDE(w_realloc, "_realloc_crt")
GEX_PROVIDE(w_recalloc, "_recalloc")
GEX_PROVIDE(w_msize, "_msize")
GEX_PROVIDE(w_expand, "_expand")
GEX_PROVIDE(w_new, "??2@YAPAXI@Z")
GEX_PROVIDE(w_new, "??_U@YAPAXI@Z")
GEX_PROVIDE(w_new, "??2@YAPAXIHPBDH@Z")
GEX_PROVIDE(w_new, "??_U@YAPAXIHPBDH@Z")
GEX_PROVIDE(w_delete, "??3@YAXPAX@Z")
GEX_PROVIDE(w_delete, "??_V@YAXPAX@Z")
GEX_PROVIDE(w_aligned_offset_realloc, "_aligned_offset_realloc")
GEX_PROVIDE(w_aligned_realloc, "_aligned_realloc")
