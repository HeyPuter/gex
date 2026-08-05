//! A content-addressed cache of compiled modules (PLAN §10).
//!
//! Translating a program to wasm is cheap — under two seconds for SQLite's
//! quarter of a million instructions. Compiling that wasm to machine code is
//! not: Cranelift takes half a minute over it, which is most of what a large
//! program's startup costs. The work is the same on every run of the same
//! binary, so it is worth doing once.
//!
//! The key is a hash of everything that could change the answer: the guest
//! image, the translation options, and a format version bumped whenever gex's
//! own output changes. What the key deliberately does *not* have to cover is
//! the engine's configuration, because [`wasmtime::Module::deserialize`]
//! checks that itself and fails rather than loading an artifact built for a
//! different wasmtime.
//!
//! # Safety
//!
//! Deserialising a compiled module is unsafe: the bytes become executable code
//! with only a header check between them and the CPU. gex trusts them for the
//! same reason it trusts its own executable — they live in a
//! per-user-writable cache directory, and anyone who can write there can
//! already replace the binary that reads it. That is the whole of the
//! argument, and it is why this crate exists rather than the `unsafe` living
//! in the host, which forbids it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use wasmtime::{Engine, Module};

pub mod sha256;

/// Bumped whenever a change to gex would make a cached artifact wrong.
///
/// The guest image is hashed, so this only has to cover gex itself. In a
/// release build it would be the crate version; during development the git
/// hash of the working tree is not available, so a mismatch is caught the
/// blunt way — by bumping this.
const FORMAT_VERSION: u32 = 1;

/// Where compiled modules live.
pub fn cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("gex").join("modules"))
}

/// The cache key for one translation.
///
/// `fields` must cover everything that changes the emitted wasm: the bytes of
/// every loaded segment, where they were placed, and the translation options.
/// The caller assembles them because the types involved live in crates this
/// one does not depend on.
///
/// Each field is length-prefixed, so no regrouping of the same bytes across
/// fields can produce the same key.
pub fn key<'a>(fields: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut h = sha256::Sha256::new();
    h.update(&FORMAT_VERSION.to_le_bytes());
    for f in fields {
        h.update(&(f.len() as u64).to_le_bytes());
        h.update(f);
    }
    sha256::hex(&h.finish())
}

/// A compiled module, either loaded from the cache or freshly built.
pub struct Loaded {
    pub module: Module,
    /// True when the compile was skipped.
    pub hit: bool,
}

/// Get the compiled form of `wasm`, using `dir` as the cache.
///
/// A cache that cannot be read or written is not an error: the module is
/// compiled and the run proceeds. That matters for a read-only home directory
/// and for two gex processes racing on the same entry.
pub fn load_or_compile(engine: &Engine, dir: &Path, key: &str, wasm: &[u8]) -> Result<Loaded> {
    let path = dir.join(format!("{key}.cwasm"));
    if let Ok(bytes) = std::fs::read(&path) {
        // SAFETY: see the module comment. `deserialize` additionally rejects
        // an artifact that does not match this engine, so a stale or truncated
        // file produces an error rather than bad code -- which is why the
        // failure path below simply recompiles.
        match unsafe { Module::deserialize(engine, &bytes) } {
            Ok(module) => return Ok(Loaded { module, hit: true }),
            Err(_) => {
                // Unusable: drop it so the next run does not pay to find out
                // again. Failing to remove it is not worth reporting.
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    let module = Module::new(engine, wasm).context("validating translated wasm")?;
    if let Ok(serialized) = module.serialize() {
        store(dir, &path, &serialized);
    }
    Ok(Loaded { module, hit: false })
}

/// Write an artifact, atomically enough that a concurrent reader never sees a
/// partial file: write to a unique name in the same directory, then rename.
fn store(dir: &Path, path: &Path, bytes: &[u8]) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!(
        "tmp-{}-{}",
        std::process::id(),
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_covers_every_field() {
        let a = key([b"program".as_slice(), b"opts".as_slice()]);
        assert_eq!(a, key([b"program".as_slice(), b"opts".as_slice()]));
        assert_ne!(a, key([b"program!".as_slice(), b"opts".as_slice()]));
        assert_ne!(a, key([b"program".as_slice(), b"opts!".as_slice()]));
        assert_ne!(a, key([b"program".as_slice()]));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn field_boundaries_cannot_be_shifted() {
        // Without length prefixes these two would hash the same bytes.
        assert_ne!(
            key([b"ab".as_slice(), b"c".as_slice()]),
            key([b"a".as_slice(), b"bc".as_slice()])
        );
    }

    #[test]
    fn a_corrupt_entry_is_replaced_rather_than_fatal() {
        let dir = std::env::temp_dir().join(format!("gex-cache-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let k = "deadbeef";
        std::fs::write(dir.join(format!("{k}.cwasm")), b"not a module").unwrap();

        let engine = Engine::default();
        // The smallest valid module.
        let wasm = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        let loaded = load_or_compile(&engine, &dir, k, &wasm).unwrap();
        assert!(!loaded.hit, "a corrupt entry must not count as a hit");

        // And the freshly stored one is usable.
        let again = load_or_compile(&engine, &dir, k, &wasm).unwrap();
        assert!(again.hit, "the store should have replaced it");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
