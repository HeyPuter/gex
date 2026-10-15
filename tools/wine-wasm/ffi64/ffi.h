/* W64's stand-in for the part of libffi libwayland uses (w3/ffi's, for
 * LP64 wasm64): ffi_call of a void function whose arguments are 32-bit
 * (ints, fixeds, fds) or 64-bit (pointers). A call_indirect must match the
 * callee's type exactly, so the call is made by argument count and the mask
 * of 64-bit arguments (ffi64.inc, made by gen.py). */
#ifndef GEX_FFI_H
#define GEX_FFI_H
typedef struct { int size; } ffi_type;
typedef struct { unsigned nargs, mask; } ffi_cif;
typedef enum { FFI_OK = 0, FFI_BAD_TYPEDEF } ffi_status;
#define FFI_DEFAULT_ABI 0
extern ffi_type ffi_type_void, ffi_type_sint32, ffi_type_uint32, ffi_type_pointer;
ffi_status ffi_prep_cif(ffi_cif *cif, int abi, unsigned nargs, ffi_type *rtype, ffi_type **atypes);
void ffi_call(ffi_cif *cif, void (*fn)(void), void *rvalue, void **avalue);
#endif
