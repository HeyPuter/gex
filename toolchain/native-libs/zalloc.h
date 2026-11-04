/* Included ahead of every zlib source (build.sh): zlib's allocator calls go
 * through rt.c, which tells zlib's own allocator from a caller's (guest
 * code). */
#include "zutil.h"
#undef ZALLOC
#undef ZFREE
voidpf gexp_zalloc(z_streamp strm, uInt items, uInt size);
void gexp_zfree(z_streamp strm, voidpf p);
#define ZALLOC(strm, items, size) gexp_zalloc((strm), (items), (size))
#define ZFREE(strm, addr) gexp_zfree((strm), (voidpf)(addr))
