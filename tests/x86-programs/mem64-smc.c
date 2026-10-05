/* smc-direct-jump.c under a 64-bit guest memory (docs/mem64.md): the block
 * guard on code in a writable mapping reads the guest's bytes at a constant
 * address, which must be an i64 there (Muck's Mono JIT met it first). */
#include "smc-direct-jump.c"
