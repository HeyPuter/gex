//! gex-eject: turn what a gex session learned into a standalone project.
//!
//! `gexc run --record DIR` writes a session manifest (the program's closure,
//! its files, its syscalls) and a profile (the code it ran). From those,
//! [`eject::eject`] writes a project directory that builds and runs without
//! the recording, the original files, or a gex checkout:
//!
//! ```text
//! project/
//!   gex-project.json    what the project is (generated)
//!   rootfs/             the closure and the files the session read (generated)
//!   session/            the manifest and profile it came from (generated)
//!   runtime/gexc        the runtime (generated: a copy of the ejecting gexc)
//!   gen/gex_replace.h   the guest ABI for replacement code (generated)
//!   build.sh, run.sh    build step and native launcher (generated)
//!   user/               yours: eject.json, src/*.c, files/ (never rewritten)
//!   build/              build output (build.sh)
//! ```
//!
//! [`build::build`] translates each program image ahead of time into a tier-1
//! module, seeded with every block the session ran, and binds the user's
//! library replacements (`user/eject.json`: a guest function, by object and
//! symbol and checked against the object's hash, served by a user wasm
//! function). [`run::run`] runs the result with no runtime translation: code
//! the build never saw fails loudly unless `--jit-fallback` is given.
//!
//! The design and its limits are in `docs/eject.md`.

pub mod build;
pub mod config;
pub mod eject;
pub mod json;
pub mod plt;
pub mod project;
pub mod run;
pub mod templates;
