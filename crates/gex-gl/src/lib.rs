//! OpenGL ES for a gex guest, by running it on the host's OpenGL ES.
//!
//! See [`proto`] for why the split is at the API rather than at a virtual
//! GPU, and [`host`] for what runs the commands.

pub mod proto;

#[cfg(not(target_arch = "wasm32"))]
pub mod host;
