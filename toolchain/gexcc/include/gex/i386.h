/* i386 code in a wasm32 program: link with -fgex-guest-i386 (gexcc's
 * rt/i386.c). The program writes i386 code into its memory (executable:
 * mprotect it PROT_READ|PROT_EXEC after writing, or map it RWX), calls it
 * with gex_i386_call, and is called back from it through entries bound at
 * guest addresses. A guest address is a pointer of the program's own. */
#ifndef GEX_I386_H
#define GEX_I386_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Call the i386 function at `target` with `n` 32-bit stack words (cdecl or
 * stdcall: the stack pointer is put back whatever it popped). EDX:EAX. */
uint64_t gex_i386_call(uint32_t target, const uint32_t *words, unsigned n);
/* The same for a floating-point result (ST0). */
double gex_i386_call_fp(uint32_t target, const uint32_t *words, unsigned n);

/* A body i386 code calls: gex's function-table signature. `ctx` is the
 * guest thread's context; read the arguments with gex_i386_arg and end
 * with gex_i386_return. */
typedef int (*gex_i386_body_t)(int ctx, int32_t depth, uint32_t need, uint64_t ret);

/* Run `body` on the guest thread's stack (what an entry must do before it
 * runs C code): GEX_I386_ENTRY writes the call. */
int gex_i386_enter(int ctx, int32_t depth, uint32_t need, uint64_t ret, gex_i386_body_t body);

/* NAME, an entry whose C body is NAME_body. */
#define GEX_I386_ENTRY(name)                                                       \
    static int name##_body(int ctx, int32_t depth, uint32_t need, uint64_t ret);   \
    int name(int ctx, int32_t depth, uint32_t need, uint64_t ret)                  \
    {                                                                              \
        return gex_i386_enter(ctx, depth, need, ret, name##_body);                 \
    }                                                                              \
    static int name##_body(int ctx, int32_t depth, uint32_t need, uint64_t ret)

/* i386 code calling or jumping to `addr` runs `entry` (an entry made with
 * GEX_I386_ENTRY). 0, or -1 when gex refuses the address. */
int gex_i386_bind(uint32_t addr, gex_i386_body_t entry);

/* Stack argument `i` (0-based, past the return address). */
uint32_t gex_i386_arg(int ctx, unsigned i);
/* Return to the guest with EDX:EAX, popping the return address and `pop`
 * bytes of arguments (0 for cdecl). */
int gex_i386_return(int ctx, uint64_t ret, unsigned pop, uint64_t edx_eax);
/* The same with a floating-point result in ST0. */
int gex_i386_return_fp(int ctx, uint64_t ret, unsigned pop, double v);

#ifdef __cplusplus
}
#endif

#endif
