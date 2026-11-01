#include <stdio.h>
#include <stdlib.h>
#include "ffi.h"
#include "ffi64.inc"
ffi_type ffi_type_void = {0}, ffi_type_sint32 = {4}, ffi_type_uint32 = {4}, ffi_type_pointer = {sizeof(void *)};
ffi_status ffi_prep_cif(ffi_cif *cif, int abi, unsigned nargs, ffi_type *rtype, ffi_type **atypes)
{
    if (nargs > 32) return FFI_BAD_TYPEDEF;
    cif->mask = 0;
    for (unsigned i = 0; i < nargs; i++)
    {
        if (atypes[i]->size == 8) cif->mask |= 1u << i;
        else if (atypes[i]->size != 4) return FFI_BAD_TYPEDEF;
    }
    cif->nargs = nargs;
    return FFI_OK;
}
void ffi_call(ffi_cif *cif, void (*fn)(void), void *rvalue, void **avalue)
{
    void (*call)(void (*)(void), void **) = ffi64_pick(cif->nargs, cif->mask);
    if (call) { call(fn, avalue); return; }
    fprintf(stderr, "ffi: no call for %u arguments, 64-bit mask %#x (ffi64/gen.py)\n", cif->nargs, cif->mask);
    abort();
}
