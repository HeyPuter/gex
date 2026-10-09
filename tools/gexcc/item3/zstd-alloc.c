/* zstd's custom allocator calls (build-zstd.sh points allocations.h here).
 * A ZSTD_customMem the caller gave is guest code, called as such; with none,
 * zstd uses rt.c's malloc and free (ZSTD_malloc/ZSTD_free). A context made
 * with one keeps it, so neither heap ever frees the other's blocks. */
#include "item3.h"
#define ZSTD_STATIC_LINKING_ONLY
#include "zstd.h"
void *gexp_zstd_alloc(ZSTD_customMem m, size_t size)
{
	uint64_t a[2] = { (uint64_t)(uintptr_t)m.opaque, size };
	return (void *)(uintptr_t)a64_call_guest((uint64_t)(uintptr_t)m.customAlloc, 2, a);
}
void gexp_zstd_free(ZSTD_customMem m, void *p)
{
	uint64_t a[2] = { (uint64_t)(uintptr_t)m.opaque, (uint64_t)(uintptr_t)p };
	a64_call_guest((uint64_t)(uintptr_t)m.customFree, 2, a);
}

/* Built without ZSTD_MULTITHREAD: no pool (ZSTD_CCtx_refThreadPool then
 * has nothing to use, as with nbWorkers 0). */
ZSTD_threadPool *ZSTD_createThreadPool(size_t numThreads) { (void)numThreads; return 0; }
void ZSTD_freeThreadPool(ZSTD_threadPool *pool) { (void)pool; }
