//! The hosted block: gex's fixed structures (`gex_abi::hosted`), in the host's `.bss`.
//!
//! A static, so the host's link places it: its address is the same in every run of one build of the host, which is
//! what a cache of translations keyed by `gex_abi::layout_key` will need.

use gex_abi::hosted;

#[repr(C, align(65536))]
struct Block([u8; hosted::SIZE as usize]);

const _: () = assert!(hosted::ALIGN == 65536);

static mut BLOCK: Block = Block([0; hosted::SIZE as usize]);

/// The block's address.
pub fn base() -> u64 {
    core::ptr::addr_of!(BLOCK) as usize as u64
}

/// Make the block the layout everything is translated against. Idempotent.
pub fn init(floor: u64) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| gex_abi::set_hosted_layout(base(), floor));
}

/// How many return pads there are: one per nesting depth of host-to-guest calls.
pub const PADS: u32 = (hosted::PADS_BYTES / 4) as u32;

/// The return pad for nesting depth `depth` (1-based). Never executable: a return there misses, and the miss says the
/// call at that depth returned.
pub fn pad(depth: u32) -> u64 {
    assert!(depth >= 1 && depth < PADS, "host-to-guest calls nested {depth} deep");
    base() + hosted::PADS + 4 * depth as u64
}

/// The depth a return pad stands for, if `pc` is one.
pub fn pad_depth(pc: u64) -> Option<u32> {
    let lo = base() + hosted::PADS;
    (pc >= lo && pc < lo + hosted::PADS_BYTES && pc % 4 == 0).then(|| ((pc - lo) / 4) as u32)
}

/// How many host-function thunks there can be.
pub const THUNKS: u32 = (hosted::THUNKS_BYTES / 4) as u32;

/// The guest address of thunk `id`.
pub fn thunk(id: u32) -> u64 {
    assert!(id < THUNKS);
    base() + hosted::THUNKS + 4 * id as u64
}

/// The thunk region, as an executable range.
pub fn thunk_range() -> (u64, u64) {
    (base() + hosted::THUNKS, base() + hosted::THUNKS + hosted::THUNKS_BYTES)
}
