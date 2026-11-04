/* zlib's allocator calls (zalloc.h).
 * The stream's zalloc/zfree are either zlib's own (zcalloc, zcfree: this
 * module's functions, which zlib stores when the caller leaves them null)
 * or the caller's: guest code, called as such. A block from this module's
 * heap is never handed to the guest's free, nor a guest block to ours:
 * zcfree frees only what lies in the heap's chunks. */
#include "native-libs.h"
#include "zutil.h"
voidpf gexp_zalloc(z_streamp strm, uInt items, uInt size)
{
	if (strm->zalloc == zcalloc) return zcalloc(strm->opaque, items, size);
	uint64_t a[3] = { (uint64_t)(uintptr_t)strm->opaque, items, size };
	return (voidpf)(uintptr_t)a64_call_guest((uint64_t)(uintptr_t)strm->zalloc, 3, a);
}
void gexp_zfree(z_streamp strm, voidpf p)
{
	if (strm->zfree == zcfree) {
		if (gexp_heap_owns(p)) zcfree(strm->opaque, p);
		return;
	}
	uint64_t a[2] = { (uint64_t)(uintptr_t)strm->opaque, (uint64_t)(uintptr_t)p };
	a64_call_guest((uint64_t)(uintptr_t)strm->zfree, 2, a);
}
