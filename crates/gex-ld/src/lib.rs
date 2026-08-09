//! `gex-ld` — the loader.
//!
//! At M0 this covers what a static or static-PIE aarch64 ELF needs: parse the
//! program headers, place the segments in the guest address space, apply
//! `R_AARCH64_RELATIVE` relocations, and build the initial stack with argv,
//! envp and the auxiliary vector. The dynamic-linking surface described in
//! PLAN §6.4 grows from these same structures.

#![forbid(unsafe_code)]

pub mod closure;
pub mod dynamic;
pub mod elf;
pub mod stack;

pub use dynamic::{load_program, Program};
pub use elf::{Image, LoadError, Segment, Symbol};
pub use stack::{build_initial_stack, Identity, StackLayout};
