#include <stdlib.h>
#include "ffi.h"
ffi_type ffi_type_void = {0}, ffi_type_sint32 = {4}, ffi_type_uint32 = {4}, ffi_type_pointer = {4};
ffi_status ffi_prep_cif(ffi_cif *cif, int abi, unsigned nargs, ffi_type *rtype, ffi_type **atypes)
{
    for (unsigned i = 0; i < nargs; i++) if (atypes[i]->size != 4) return FFI_BAD_TYPEDEF;
    cif->nargs = nargs;
    return FFI_OK;
}
void ffi_call(ffi_cif *cif, void (*fn)(void), void *rvalue, void **avalue)
{
#define A(i) (*(int *)avalue[i])
    switch (cif->nargs)
    {
    case 0: ((void (*)(void))fn)(); return;
    case 1: ((void (*)(int))fn)(A(0)); return;
    case 2: ((void (*)(int, int))fn)(A(0), A(1)); return;
    case 3: ((void (*)(int, int, int))fn)(A(0), A(1), A(2)); return;
    case 4: ((void (*)(int, int, int, int))fn)(A(0), A(1), A(2), A(3)); return;
    case 5: ((void (*)(int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4)); return;
    case 6: ((void (*)(int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5)); return;
    case 7: ((void (*)(int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6)); return;
    case 8: ((void (*)(int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7)); return;
    case 9: ((void (*)(int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8)); return;
    case 10: ((void (*)(int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9)); return;
    case 11: ((void (*)(int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10)); return;
    case 12: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11)); return;
    case 13: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12)); return;
    case 14: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13)); return;
    case 15: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13), A(14)); return;
    case 16: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13), A(14), A(15)); return;
    case 17: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13), A(14), A(15), A(16)); return;
    case 18: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13), A(14), A(15), A(16), A(17)); return;
    case 19: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13), A(14), A(15), A(16), A(17), A(18)); return;
    case 20: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13), A(14), A(15), A(16), A(17), A(18), A(19)); return;
    case 21: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13), A(14), A(15), A(16), A(17), A(18), A(19), A(20)); return;
    case 22: ((void (*)(int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int))fn)(A(0), A(1), A(2), A(3), A(4), A(5), A(6), A(7), A(8), A(9), A(10), A(11), A(12), A(13), A(14), A(15), A(16), A(17), A(18), A(19), A(20), A(21)); return;
    }
    abort();
}
