//! `gex-emit` — the WebAssembly backend.
//!
//! [`wasm`] is a dependency-free binary writer; [`tier1`] turns one GIR block
//! into one wasm function; [`module`] assembles blocks, imports, the guest
//! memory and the PC-indexed dispatch table into a loadable module.

#![forbid(unsafe_code)]

pub mod dense;
pub mod merge;
pub mod module;
pub mod fma;
pub mod par;
pub mod profile;
pub mod region;
pub mod tier1;
pub mod wasm;

pub use module::{ModuleBuilder, PageTable, TranslatedModule};
pub use tier1::set_trace_blocks;
