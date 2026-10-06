/* PROVIDES libc.so.6 x86_64 stateful
 *
 * glibc's heap (x86-64) as native wasm (docs/elf-providers.md §5): dlmalloc
 * (2.8.6, as crt_heap.c) in place of ptmalloc, the whole family or none of
 * it (GEX_GROUP below): two allocators over one heap are fatal. Bound by
 * address, so the library's own calls (strdup, fopen, the dynamic linker
 * once relocated) allocate here too. Functions that only report on
 * ptmalloc's arenas (mallinfo, malloc_stats, malloc_trim, mallopt,
 * malloc_info) stay the library's and see an empty heap.
 *
 * The heap grows over guest pages (gex.provider_alloc_top: an anonymous
 * mapping in a Linux process), never given back. A pointer it did not hand
 * out (the dynamic linker's start-up allocations, which glibc never frees
 * either) is reported once; free leaves it, and realloc and
 * malloc_usable_size stop the run rather than guess its size. Failures set
 * errno (ENOMEM, EINVAL) through the library's __errno_location. One lock
 * for the heap.
 *
 * NOT REGISTERED (provider.rs SOURCES): it agrees with glibc
 * (tests/elf-prov/malloc.c) but measured no faster than translated
 * ptmalloc, whose tcache fast path is short: 400000 malloc/free pairs took
 * 177 ms with it against 175 ms without (median of 12 ABBA runs under
 * run-node, worldmachine, 2026-10-05). Register it again when a profile
 * shows a heap-bound workload it wins on. */

__attribute__((import_module("gex"), import_name("provider_alloc_top"))) int gex_provider_alloc(int size);
__attribute__((import_module("gex"), import_name("provider_log"))) void gex_provider_log(const char *p, int n);

typedef __SIZE_TYPE__ size_t;
typedef __PTRDIFF_TYPE__ ptrdiff_t;

/* ---- the lock ---- */
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
static inline int heap_owns(uint64_t p) { return p < 0x100000000ull && ((owned[p >> 21] >> ((p >> 16) & 31)) & 1); }

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
/* glibc's: 16 on x86-64. */
#define MALLOC_ALIGNMENT 16
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

/* ---- the slow paths: errno, a foreign pointer ---- */
static uint64_t f_errno_location;

static void set_errno(int ctx, int32_t depth, uint32_t code) {
    if (!f_errno_location) {
        uint64_t b = gp64_elf_base(GP_SELF_PC());
        f_errno_location = b ? gp64_elf_sym(b, "__errno_location") : 0;
        if (!f_errno_location) {
            f_errno_location = 1;
        }
    }
    if (f_errno_location > 1) {
        uint64_t sp = gp64_reg(ctx, R_RSP), ra = gp64_reg(ctx, R_RAX);
        uint64_t e = gp64_call6(ctx, depth, f_errno_location, sp - 128, 0, 0, 0, 0, 0, 0);
        if (e) {
            *(gex_u32 *)GP64_P(e) = code;
        }
        gp64_set(ctx, R_RAX, ra);
    }
}

static void foreign(const char *what, uint64_t p) {
    static int told;
    if (!told) {
        told = 1;
        char m[] = "gex glibc_malloc: XXXXXXXXXXXXXXXXXX: a pointer this heap did not hand out (0x0000000000000000)\n";
        for (int i = 0; i < 18 && what[i]; ++i) {
            m[18 + i] = what[i];
        }
        for (int i = 0; i < 16; ++i) {
            m[76 + i] = "0123456789abcdef"[(p >> (60 - 4 * i)) & 15];
        }
        gex_provider_log(m, sizeof m - 1);
    }
}

/* Every block is a guest pointer below 4 GiB (the provider's memory is the
 * guest's): a request past what that can hold fails. */
#define MAX_REQ 0xffffffe0ull

static GEX_PROVIDER(p_malloc) {
    (void)need;
    uint64_t n = gp64_arg(ctx, 0);
    void *p = n <= MAX_REQ ? dlmalloc((size_t)n) : 0;
    if (!p) {
        set_errno(ctx, depth, ENOMEM);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)(size_t)p);
}
static GEX_PROVIDER(p_calloc) {
    (void)need;
    uint64_t a = gp64_arg(ctx, 0), b = gp64_arg(ctx, 1);
    void *p = 0;
    /* (No 128-bit multiply: it is a libcall the module cannot import.) */
    if (!b || a <= MAX_REQ / b) {
        p = dlcalloc(1, (size_t)(a * b));
    }
    if (!p) {
        set_errno(ctx, depth, ENOMEM);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)(size_t)p);
}
static GEX_PROVIDER(p_free) {
    (void)depth, (void)need;
    uint64_t p = gp64_arg(ctx, 0);
    if (p) {
        if (heap_owns(p)) {
            dlfree((void *)(size_t)p);
        } else {
            foreign("free", p);
        }
    }
    return gp64_return(ctx, ret);
}
static GEX_PROVIDER(p_realloc) {
    (void)need;
    uint64_t p = gp64_arg(ctx, 0), n = gp64_arg(ctx, 1);
    if (p && !heap_owns(p)) {
        foreign("realloc", p);
        __builtin_trap();
    }
    uint64_t r;
    if (!p) {
        r = n <= MAX_REQ ? (uint32_t)(size_t)dlmalloc((size_t)n) : 0;
    } else if (!n) {
        /* glibc frees and returns NULL. */
        dlfree((void *)(size_t)p);
        return gp64_return_rax(ctx, ret, 0);
    } else {
        r = n <= MAX_REQ ? (uint32_t)(size_t)dlrealloc((void *)(size_t)p, (size_t)n) : 0;
    }
    if (!r) {
        set_errno(ctx, depth, ENOMEM);
    }
    return gp64_return_rax(ctx, ret, r);
}
/* memalign and aligned_alloc (one function in glibc): any alignment, a
 * non-power of two rounded up, as glibc's. */
static GEX_PROVIDER(p_memalign) {
    (void)need;
    uint64_t al = gp64_arg(ctx, 0), n = gp64_arg(ctx, 1);
    void *p = 0;
    if (al > 0x80000000ull) {
        set_errno(ctx, depth, EINVAL);
        return gp64_return_rax(ctx, ret, 0);
    }
    if (n <= MAX_REQ) {
        p = dlmemalign((size_t)al, (size_t)n);
    }
    if (!p) {
        set_errno(ctx, depth, ENOMEM);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)(size_t)p);
}
static GEX_PROVIDER(p_posix_memalign) {
    (void)need;
    uint64_t out = gp64_arg(ctx, 0), al = gp64_arg(ctx, 1), n = gp64_arg(ctx, 2);
    /* EINVAL: not a power of two times sizeof(void *). */
    if (al % 8 || !al || (al & (al - 1)) || al > 0x80000000ull) {
        return gp64_return_rax(ctx, ret, EINVAL);
    }
    void *p = n <= MAX_REQ ? dlmemalign((size_t)al, (size_t)n) : 0;
    if (!p) {
        return gp64_return_rax(ctx, ret, ENOMEM);
    }
    GW64(out) = (uint32_t)(size_t)p;
    return gp64_return_rax(ctx, ret, 0);
}
static GEX_PROVIDER(p_valloc) {
    (void)need;
    uint64_t n = gp64_arg(ctx, 0);
    void *p = n <= MAX_REQ ? dlmemalign(4096, (size_t)n) : 0;
    if (!p) {
        set_errno(ctx, depth, ENOMEM);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)(size_t)p);
}
static GEX_PROVIDER(p_pvalloc) {
    (void)need;
    uint64_t n = gp64_arg(ctx, 0);
    void *p = 0;
    if (n <= MAX_REQ - 4096) {
        p = dlmemalign(4096, (size_t)((n + 4095) & ~4095ull));
    }
    if (!p) {
        set_errno(ctx, depth, ENOMEM);
    }
    return gp64_return_rax(ctx, ret, (uint32_t)(size_t)p);
}
static GEX_PROVIDER(p_usable_size) {
    (void)depth, (void)need;
    uint64_t p = gp64_arg(ctx, 0);
    if (!p) {
        return gp64_return_rax(ctx, ret, 0);
    }
    if (!heap_owns(p)) {
        foreign("malloc_usable_size", p);
        __builtin_trap();
    }
    return gp64_return_rax(ctx, ret, dlmalloc_usable_size((void *)(size_t)p));
}

GEX_GROUP("malloc,free,calloc,realloc,memalign,posix_memalign,valloc,pvalloc,malloc_usable_size")
GEX_PROVIDE(p_malloc, "malloc")
GEX_PROVIDE(p_free, "free")
GEX_PROVIDE(p_calloc, "calloc")
GEX_PROVIDE(p_realloc, "realloc")
GEX_PROVIDE(p_memalign, "memalign")
GEX_PROVIDE(p_memalign, "aligned_alloc")
GEX_PROVIDE(p_posix_memalign, "posix_memalign")
GEX_PROVIDE(p_valloc, "valloc")
GEX_PROVIDE(p_pvalloc, "pvalloc")
GEX_PROVIDE(p_usable_size, "malloc_usable_size")
