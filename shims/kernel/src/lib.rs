//! The global kernel module.
//!
//! One module, one memory, shared by every Worker of every guest process.
//! That is the whole point of it: the file table, the process table, the
//! pipes and the unix sockets live *here*, in a memory every Worker
//! instantiated against, so two processes can talk without either of them
//! being copied anywhere and a syscall stays a direct call rather than
//! becoming a message and a wait.
//!
//! Each guest process keeps its own memory for its own address space, shared
//! between that process's threads and nothing else. Two processes therefore
//! cannot put their libraries at the same addresses, which is the collision
//! the snapshot switcher exists to work around -- and with it gone there is
//! nothing to snapshot.
//!
//! A guest buffer is in the *process's* memory, which this module imports as
//! a second memory and cannot address with a pointer. `gex-kmem` copies
//! across the two with `memory.copy`, the one multi-memory operation that
//! survives the toolchain.
//!
//! This file is the seam, not the kernel: what is here is the state that has
//! to be shared and the proof that sharing it works. `gex-kernel` moves in
//! behind it.

#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

/// Somewhere in this module's own memory to stage a guest buffer.
///
/// Static rather than allocated because there is no allocator here yet and
/// because every syscall wants exactly this: a place to put the bytes it
/// copied out of a process so it can look at them.
static mut SCRATCH: [u8; 4096] = [0; 4096];

/// How many times any Worker has called in.
///
/// The simplest thing that can demonstrate the property everything else
/// depends on: this lives in the shared memory, so a second Worker's
/// instance increments the same counter rather than one of its own. If this
/// ever reads as 1 from two Workers, the memory is not shared and nothing
/// built on top of it would be either.
static CALLS: AtomicU32 = AtomicU32::new(0);

/// A word of state a process can leave for another to find.
///
/// Stands in for the process table: the point is that it is written by one
/// Worker, against one process's memory, and read by another.
static NOTE: AtomicU32 = AtomicU32::new(0);

#[no_mangle]
pub extern "C" fn k_calls() -> u32 {
    CALLS.load(Ordering::Relaxed)
}

/// Read a `u64` out of the calling process's memory and remember it.
///
/// The shape of every syscall that takes a pointer: the argument is an
/// address in a memory this module cannot dereference, so it is copied here
/// first.
#[no_mangle]
pub extern "C" fn k_note_from_guest(at: u32) -> u32 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    let v = unsafe { gex_kmem::read_u64(at) } as u32;
    NOTE.store(v, Ordering::Relaxed);
    v
}

/// Write the remembered word back into the calling process's memory.
#[no_mangle]
pub extern "C" fn k_note_to_guest(at: u32) {
    CALLS.fetch_add(1, Ordering::Relaxed);
    unsafe { gex_kmem::write_u64(at, NOTE.load(Ordering::Relaxed) as u64) };
}

/// Copy `len` bytes of the guest's memory in, and report their sum.
///
/// A buffer rather than a word, because that is the case the design is built
/// for: `write` and `read` move buffers, and moving one is a single
/// `memory.copy` rather than a loop of loads that cannot be written.
#[no_mangle]
pub extern "C" fn k_checksum(at: u32, len: u32) -> u32 {
    CALLS.fetch_add(1, Ordering::Relaxed);
    let n = (len as usize).min(4096);
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) };
    unsafe { gex_kmem::pull(buf.as_mut_ptr(), at, n as u32) };
    let mut sum: u32 = 0;
    for b in &buf[..n] {
        sum = sum.wrapping_mul(31).wrapping_add(*b as u32);
    }
    sum
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
