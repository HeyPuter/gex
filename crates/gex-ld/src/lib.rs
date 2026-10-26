//! `gex-ld` — the loader.
//!
//! Parses program headers, places segments in the guest address space, applies
//! `R_AARCH64_RELATIVE` relocations, and builds the initial stack (argv, envp,
//! auxiliary vector). Dynamic linking grows from these structures.
//! [`pe`] loads Windows PE32/PE32+ images and their DLL closures.

#![forbid(unsafe_code)]

pub mod closure;
pub mod dynamic;
pub mod elf;
pub mod link;
pub mod pe;
pub mod stack;

pub use dynamic::{load_program, plan_program, Program};
pub use elf::{Image, LoadError, Segment, Symbol};
pub use link::{link, Linked, Object};
pub use stack::{build_initial_stack, Identity, StackLayout};
