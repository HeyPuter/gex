/* gex-embed: gex's AArch64 translator hosted in this program's memory (gex/shims/embed). */
#ifndef GEX_EMBED_H
#define GEX_EMBED_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct gex_embed_config {
    uint32_t floor;      /* the lowest address the guest may name: GLOBAL_BASE */
    uint32_t stack_size; /* each thread's guest stack (0: 1 MiB) */
} gex_embed_config;

/* Set gex up. Once, before anything else here. */
int gex_embed_init(const gex_embed_config *cfg);

/* Guest code may run in [addr, addr + len). */
void gex_embed_map_exec(uint64_t addr, uint64_t len);

/* A host function the guest calls: reads its arguments from the context (gex_ctx_x, gex_ctx_d) and writes its results
 * there. Static functions only: the guest reaches them by function-table index, which a function added at run time
 * (addFunction) does not have on every Worker. */
typedef void (*gex_host_fn)(uint32_t ctx, uint32_t user);

/* The guest address that, called, runs `f`. */
uint64_t gex_embed_thunk(gex_host_fn f, uint32_t user);

/* Start the guest on this thread: bionic's linker64 at `linker`, loading `exe` (a program that only has libc loaded).
 * This thread is the guest's main thread after. 0, or -1 with the reason logged. */
int gex_embed_boot(const char *linker, const char *exe);

/* The guest's dlopen, dlsym and dlerror (the linker's, in the default namespace). */
uint64_t gex_embed_dlopen(const char *path, int flags);
uint64_t gex_embed_dlsym(uint64_t handle, const char *name);
uint32_t gex_embed_dlerror(void);

/* The default namespace's library search path (android_update_LD_LIBRARY_PATH): colon-separated directories searched
 * for libraries loaded by name, an app's own among them. */
void gex_embed_set_library_path(const char *paths);

/* A library whose functions are the host's: when one named `soname` is mapped executable, each of `fns` is bound by
 * name, and, unless `missing` is null, every other function it exports is bound to `missing`, with "soname!symbol"
 * as its user value. Before the library is loaded. */
typedef struct gex_provided {
    const char *name;
    gex_host_fn fn;
    uint32_t user;
} gex_provided;
void gex_embed_provide(const char *soname, const gex_provided *fns, uint32_t n, gex_host_fn missing);

/* A `missing` for gex_embed_provide: says which function, once, and returns 0. */
void gex_embed_missing(uint32_t ctx, uint32_t label);

/* Say every guest system call on stderr. */
void gex_embed_set_trace(int on);

/* A guest call's arguments and results, by the AAPCS64. */
typedef struct gex_call {
    uint64_t x[8];
    uint64_t v[8][2];
    uint64_t x8;
    const uint64_t *stack; /* arguments past the registers, 8 bytes each, lowest address first */
    uint32_t nstack;
    uint64_t rx[2];        /* results: x0, x1 */
    uint64_t rv[4][2];     /* results: v0-v3 */
} gex_call;

/* Call the guest function at `pc` on this thread. */
int gex_embed_call(uint64_t pc, gex_call *c);

/* Registers of a context (31 is SP) and the low halves of its vector registers (d0-d31). */
uint64_t gex_ctx_x(uint32_t ctx, uint32_t n);
void gex_ctx_set_x(uint32_t ctx, uint32_t n, uint64_t v);
uint64_t gex_ctx_d(uint32_t ctx, uint32_t n);
void gex_ctx_set_d(uint32_t ctx, uint32_t n, uint64_t v);

#ifdef __cplusplus
}
#endif

#endif
