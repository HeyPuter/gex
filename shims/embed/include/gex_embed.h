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
