/* libffi for gexcc's wasm32 (not emscripten): ffi_call in plain C.

   Replaces src/wasm32/ffi.c, whose emscripten port makes its calls in
   JavaScript (EM_JS); a gexcc program has no JavaScript of its own and runs
   natively under wasmtime as well as in the page.

   A wasm call_indirect names the callee's exact signature, so the call goes
   through one of the trampolines gen.py writes (tramp.inc), chosen by the
   signature the cif lowers to. Lowering follows clang's wasm32 C ABI:

   - integers up to 32 bits, pointers: i32, extended by the caller;
     64-bit integers: i64; float: f32; double: f64;
   - long double (binary128): two i64 as an argument, returned through a
     hidden pointer;
   - a struct or union with one scalar inside (after unwrapping): as that
     scalar; any other aggregate as a pointer to a copy, and returned through
     a hidden first pointer argument;
   - variadic arguments: one i32 pointer to a buffer holding them in order,
     each at its type's alignment (aggregates as pointers).

   Closures are not supported: a closure's code pointer is a table index of
   a function with the closure's signature, and that has to be chosen before
   the signature is known (ffi_closure_alloc). ffi_closure_alloc returns
   NULL. */

#include <ffi.h>
#include <ffi_common.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef void (*tramp_fn)(void (*fn)(void), const uint64_t *a, void *ret);
#include "tramp.inc"

#define MAX_PARAMS 64

/* The scalar a single-element aggregate is passed as, or NULL. */
static ffi_type *
single_scalar (ffi_type *t)
{
  while (t->type == FFI_TYPE_STRUCT)
    {
      ffi_type **e = t->elements;
      ffi_type *only = NULL;
      if (!e)
        return NULL;
      for (; *e; e++)
        {
          /* empty members do not count */
          if ((*e)->type == FFI_TYPE_STRUCT && (*e)->size == 0)
            continue;
          if (only)
            return NULL;
          only = *e;
        }
      if (!only)
        return NULL;
      t = only;
    }
  return t;
}

/* The wasm value class of a scalar: 'I', 'J', 'F', 'D', 'L' (long double:
   two i64), or 0 for one we do not lower. */
static char
scalar_class (ffi_type *t)
{
  switch (t->type)
    {
    case FFI_TYPE_INT:
    case FFI_TYPE_UINT8:
    case FFI_TYPE_SINT8:
    case FFI_TYPE_UINT16:
    case FFI_TYPE_SINT16:
    case FFI_TYPE_UINT32:
    case FFI_TYPE_SINT32:
    case FFI_TYPE_POINTER:
      return 'I';
    case FFI_TYPE_UINT64:
    case FFI_TYPE_SINT64:
      return 'J';
    case FFI_TYPE_FLOAT:
      return 'F';
    case FFI_TYPE_DOUBLE:
      return 'D';
#if FFI_TYPE_LONGDOUBLE != FFI_TYPE_DOUBLE
    case FFI_TYPE_LONGDOUBLE:
      return 'L';
#endif
    default:
      return 0;
    }
}

/* How a value of type t travels as an argument: a scalar class, or 'P' for
   a pointer to a copy. */
static char
arg_class (ffi_type *t)
{
  ffi_type *s = single_scalar (t);
  if (s)
    return scalar_class (s);
  return t->type == FFI_TYPE_STRUCT ? 'P' : 0;
}

static int
cmp_sig (const void *key, const void *elem)
{
  return strcmp ((const char *) key, ((const struct tramp *) elem)->sig);
}

/* Build the cif's wasm signature (cif->flags holds the trampoline's index). */
static ffi_status
lower (ffi_cif *cif, unsigned nfixed, int variadic)
{
  char sig[MAX_PARAMS + 2];
  unsigned n = 1, i;
  ffi_type *rt = cif->rtype;
  ffi_type *rs = single_scalar (rt);
  char rc;

  if (rt->type == FFI_TYPE_VOID)
    rc = 'V';
  else if (rs && scalar_class (rs) && scalar_class (rs) != 'L')
    rc = scalar_class (rs);
  else if ((rs && scalar_class (rs) == 'L') || rt->type == FFI_TYPE_STRUCT)
    {
      rc = 'V';
      sig[n++] = 'I'; /* the hidden return pointer */
    }
  else
    return FFI_BAD_TYPEDEF;
  sig[0] = rc;

  for (i = 0; i < nfixed; i++)
    {
      char c = arg_class (cif->arg_types[i]);
      if (!c)
        return FFI_BAD_TYPEDEF;
      if (n + 2 > MAX_PARAMS)
        return FFI_BAD_TYPEDEF;
      if (c == 'L')
        {
          sig[n++] = 'J';
          sig[n++] = 'J';
        }
      else
        sig[n++] = c == 'P' ? 'I' : c;
    }
  if (variadic)
    {
      for (i = nfixed; i < cif->nargs; i++)
        if (!arg_class (cif->arg_types[i]))
          return FFI_BAD_TYPEDEF;
      sig[n++] = 'I'; /* the buffer */
    }
  sig[n] = 0;

  const struct tramp *t = bsearch (sig, tramps, sizeof tramps / sizeof *tramps,
                                   sizeof *tramps, cmp_sig);
  if (!t)
    return FFI_BAD_TYPEDEF;
  cif->flags = (unsigned) (t - tramps);
  cif->nfixedargs = variadic ? nfixed : cif->nargs;
  /* bytes ffi_call needs for copies and the variadic buffer: generous */
  cif->bytes = 0;
  return FFI_OK;
}

ffi_status FFI_HIDDEN
ffi_prep_cif_machdep (ffi_cif *cif)
{
  if (cif->abi != FFI_WASM32)
    return FFI_BAD_ABI;
  return lower (cif, cif->nargs, 0);
}

ffi_status FFI_HIDDEN
ffi_prep_cif_machdep_var (ffi_cif *cif, unsigned nfixedargs,
                          unsigned ntotalargs)
{
  (void) ntotalargs;
  if (cif->abi != FFI_WASM32)
    return FFI_BAD_ABI;
  return lower (cif, nfixedargs, 1);
}

/* One integer argument, extended to 32 bits as the caller must. */
static uint32_t
load_int (ffi_type *t, const void *p)
{
  switch (t->type)
    {
    case FFI_TYPE_UINT8:
      return *(const uint8_t *) p;
    case FFI_TYPE_SINT8:
      return (uint32_t) (int32_t) * (const int8_t *) p;
    case FFI_TYPE_UINT16:
      return *(const uint16_t *) p;
    case FFI_TYPE_SINT16:
      return (uint32_t) (int32_t) * (const int16_t *) p;
    default:
      return *(const uint32_t *) p;
    }
}

static size_t
align_up (size_t v, size_t a)
{
  return a ? (v + a - 1) & ~(a - 1) : v;
}

void
ffi_call (ffi_cif *cif, void (*fn) (void), void *rvalue, void **avalue)
{
  uint64_t a[MAX_PARAMS];
  unsigned n = 0, i;
  ffi_type *rt = cif->rtype;
  ffi_type *rs = single_scalar (rt);
  int sret = rt->type != FFI_TYPE_VOID
             && !(rs && scalar_class (rs) && scalar_class (rs) != 'L');
  /* Copies of aggregates passed by pointer, the variadic buffer, and a
     return slot when the caller gave none. */
  size_t room = 16 + align_up (rt->size, 16);
  for (i = 0; i < cif->nargs; i++)
    room += align_up (cif->arg_types[i]->size, 16) + 16;
  unsigned char *scratch = __builtin_alloca (room);
  size_t used = 0;
  uint64_t rbuf[4] = { 0 };
  void *rp = rvalue;

  if (sret)
    {
      if (!rp)
        {
          rp = scratch;
          used = align_up (rt->size, 16);
        }
      a[n++] = (uint32_t) (uintptr_t) rp;
    }

  for (i = 0; i < cif->nfixedargs; i++)
    {
      ffi_type *t = cif->arg_types[i];
      ffi_type *s = single_scalar (t);
      char c = arg_class (t);
      const void *p = avalue[i];
      switch (c)
        {
        case 'I':
          a[n++] = load_int (s, p);
          break;
        case 'J':
          a[n++] = *(const uint64_t *) p;
          break;
        case 'F':
          a[n] = 0;
          memcpy (&a[n++], p, 4);
          break;
        case 'D':
          memcpy (&a[n++], p, 8);
          break;
        case 'L':
          memcpy (&a[n], p, 16);
          n += 2;
          break;
        case 'P':
          {
            used = align_up (used, t->alignment > 16 ? 16 : t->alignment);
            void *copy = scratch + used;
            memcpy (copy, p, t->size);
            used += t->size;
            a[n++] = (uint32_t) (uintptr_t) copy;
            break;
          }
        }
    }

  if (cif->nfixedargs < cif->nargs)
    {
      /* Copies of aggregates first, then the buffer that points at them. */
      void *copies[MAX_PARAMS];
      for (i = cif->nfixedargs; i < cif->nargs && i < MAX_PARAMS; i++)
        {
          ffi_type *t = cif->arg_types[i];
          copies[i] = NULL;
          if (arg_class (t) == 'P')
            {
              used = align_up (used, t->alignment > 16 ? 16 : t->alignment);
              copies[i] = scratch + used;
              memcpy (copies[i], avalue[i], t->size);
              used += t->size;
            }
        }
      used = align_up (used, 16);
      unsigned char *buf = scratch + used;
      size_t off = 0;
      for (i = cif->nfixedargs; i < cif->nargs && i < MAX_PARAMS; i++)
        {
          ffi_type *t = cif->arg_types[i];
          ffi_type *s = single_scalar (t);
          char c = arg_class (t);
          switch (c)
            {
            case 'I':
              {
                uint32_t v = load_int (s, avalue[i]);
                off = align_up (off, 4);
                memcpy (buf + off, &v, 4);
                off += 4;
                break;
              }
            case 'F':
              /* promoted to double by C; a caller that passed float anyway */
              {
                double d = *(const float *) avalue[i];
                off = align_up (off, 8);
                memcpy (buf + off, &d, 8);
                off += 8;
                break;
              }
            case 'J':
            case 'D':
              off = align_up (off, 8);
              memcpy (buf + off, avalue[i], 8);
              off += 8;
              break;
            case 'L':
              off = align_up (off, 16);
              memcpy (buf + off, avalue[i], 16);
              off += 16;
              break;
            case 'P':
              {
                uint32_t v = (uint32_t) (uintptr_t) copies[i];
                off = align_up (off, 4);
                memcpy (buf + off, &v, 4);
                off += 4;
                break;
              }
            }
        }
      a[n++] = (uint32_t) (uintptr_t) buf;
    }

  tramps[cif->flags].fn (fn, a, rbuf);

  if (sret || rt->type == FFI_TYPE_VOID || !rvalue)
    return;
  /* A scalar result: integers narrower than ffi_arg widen to it. */
  switch (rs->type)
    {
    case FFI_TYPE_UINT8:
    case FFI_TYPE_UINT16:
    case FFI_TYPE_UINT32:
    case FFI_TYPE_INT:
    case FFI_TYPE_SINT8:
    case FFI_TYPE_SINT16:
    case FFI_TYPE_SINT32:
    case FFI_TYPE_POINTER:
      if (rt->type == FFI_TYPE_STRUCT)
        memcpy (rvalue, rbuf, rt->size);
      else
        {
          uint32_t v = (uint32_t) rbuf[0];
          switch (rs->type)
            {
            case FFI_TYPE_UINT8: v = (uint8_t) v; break;
            case FFI_TYPE_SINT8: v = (uint32_t) (int32_t) (int8_t) v; break;
            case FFI_TYPE_UINT16: v = (uint16_t) v; break;
            case FFI_TYPE_SINT16: v = (uint32_t) (int32_t) (int16_t) v; break;
            default: break;
            }
          *(ffi_arg *) rvalue = v;
        }
      break;
    default:
      memcpy (rvalue, rbuf, rt->size < 8 ? rt->size : 8);
      break;
    }
}

void *
ffi_closure_alloc (size_t size, void **code)
{
  (void) size;
  if (code)
    *code = NULL;
  return NULL;
}

void
ffi_closure_free (void *ptr)
{
  (void) ptr;
}

ffi_status
ffi_prep_closure_loc (ffi_closure *closure, ffi_cif *cif,
                      void (*fun) (ffi_cif *, void *, void **, void *),
                      void *user_data, void *codeloc)
{
  (void) closure; (void) cif; (void) fun; (void) user_data; (void) codeloc;
  return FFI_BAD_ABI;
}

void *
ffi_data_to_code_pointer (void *data)
{
  return data;
}

int
ffi_tramp_is_present (void *ptr)
{
  (void) ptr;
  return 0;
}
