//! The global allocator: the host's `malloc`.
//!
//! Never Rust's own wasm allocator. That one grows the memory with `memory.grow`, behind the host's `sbrk`, and two
//! heaps growing one memory hand out the same pages twice.

#[cfg(target_arch = "wasm32")]
mod imp {
    use core::alloc::{GlobalAlloc, Layout};

    extern "C" {
        fn malloc(size: usize) -> *mut u8;
        fn calloc(n: usize, size: usize) -> *mut u8;
        fn realloc(p: *mut u8, size: usize) -> *mut u8;
        fn memalign(align: usize, size: usize) -> *mut u8;
        fn free(p: *mut u8);
    }

    /// What emscripten's dlmalloc guarantees without being asked (`MALLOC_ALIGNMENT`, `max_align_t`: 8). Anything
    /// wider goes to `memalign`.
    const MIN_ALIGN: usize = 8;

    struct HostMalloc;

    unsafe impl GlobalAlloc for HostMalloc {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            if l.align() <= MIN_ALIGN { malloc(l.size()) } else { memalign(l.align(), l.size()) }
        }

        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            if l.align() <= MIN_ALIGN {
                return calloc(1, l.size());
            }
            let p = memalign(l.align(), l.size());
            if !p.is_null() {
                p.write_bytes(0, l.size());
            }
            p
        }

        unsafe fn dealloc(&self, p: *mut u8, _: Layout) {
            free(p)
        }

        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            if l.align() <= MIN_ALIGN {
                return realloc(p, new);
            }
            // dlmalloc's realloc keeps only its own alignment.
            let q = memalign(l.align(), new);
            if !q.is_null() {
                q.copy_from_nonoverlapping(p, l.size().min(new));
                free(p);
            }
            q
        }
    }

    #[global_allocator]
    static ALLOC: HostMalloc = HostMalloc;
}
