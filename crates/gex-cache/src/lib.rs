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
use std::sync::atomic::{AtomicU64, Ordering};
use wasmtime::{Engine, Module};

/// How much disk the cache may use, before it starts discarding the entries
/// it has gone longest without reading.
///
/// It needs a limit because most of what lands here is never read again. A
/// whole program keys on its own bytes and hits on every later run, which is
/// the case this cache was built for; a batch of runtime-translated blocks
/// keys on *which blocks were in the batch*, and that depends on the order
/// discovery happened to reach them, so it almost never recurs. Those entries
/// are written once and read never.
///
/// Measured on this machine after a few days of graphics work: 247,684
/// entries, 21 GB, of which 88% were under 64 KB -- runtime batches. Nothing
/// removed any of them.
const DEFAULT_BUDGET: u64 = 4 << 30;

/// Sweeping means listing the directory, so it is not done on every store.
const STORES_BETWEEN_SWEEPS: u64 = 64;
static STORES: AtomicU64 = AtomicU64::new(0);

fn budget() -> u64 {
    std::env::var("GEX_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BUDGET)
}

/// Delete the least recently used entries until the cache is comfortably
/// under budget.
///
/// "Least recently used" is the metadata file's modification time, which
/// [`load_or_compile`] refreshes on a hit. Sorting by it therefore keeps the
/// modules that are actually being reused and drops the ones that are not,
/// which is precisely the split between whole programs and runtime batches.
///
/// Down to 80% rather than exactly to the limit, so a run does not sweep on
/// every store once it is full.
fn evict(dir: &Path, budget: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // (mtime, key, bytes) for each entry, sized as module plus metadata.
    let mut items: Vec<(std::time::SystemTime, String, u64)> = Vec::new();
    let mut total = 0u64;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("cwasm") {
            continue;
        }
        let Some(key) = path.file_stem().and_then(|k| k.to_str()).map(String::from) else {
            continue;
        };
        let wasm_len = e.metadata().map(|m| m.len()).unwrap_or(0);
        let meta_path = dir.join(format!("{key}.meta"));
        let meta = std::fs::metadata(&meta_path);
        let meta_len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let when = meta
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        total += wasm_len + meta_len;
        items.push((when, key, wasm_len + meta_len));
    }
    if total <= budget {
        return;
    }
    items.sort_by_key(|(when, _, _)| *when);
    let target = budget / 5 * 4;
    for (_, key, size) in items {
        if total <= target {
            break;
        }
        let _ = std::fs::remove_file(dir.join(format!("{key}.cwasm")));
        let _ = std::fs::remove_file(dir.join(format!("{key}.meta")));
        total = total.saturating_sub(size);
    }
}

pub mod sha256;

/// Bumped whenever the *layout* of a cache entry changes.
///
/// What gex itself does to the guest is covered by [`build_fingerprint`], not
/// by this.
const FORMAT_VERSION: u32 = 2;

/// Identifies the running gex build, so a change to the translator cannot be
/// served stale code from a previous one.
///
/// This is not optional. A decoder fix that makes an instruction translatable
/// changes nothing else the key can see — not the guest image, not the
/// options — so without it the fixed binary reuses the broken module. That is
/// exactly what happened the first time this cache existed, and the failure
/// looked like the fix had not worked.
///
/// The executable's size and modification time rather than a hash of it: gexc
/// is tens of megabytes, and a rebuild always changes both. `None` means the
/// cache must not be used at all, because a build that cannot be identified
/// cannot be told apart from a different one.
pub fn build_fingerprint() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let meta = std::fs::metadata(&exe).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(format!(
        "{}:{}.{}",
        meta.len(),
        mtime.as_secs(),
        mtime.subsec_nanos()
    ))
}

fn gex_cache_root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("gex"))
}

/// Where compiled modules live.
pub fn cache_dir() -> Option<PathBuf> {
    Some(gex_cache_root()?.join("modules"))
}

/// Where entry-point profiles live.
pub fn profile_dir() -> Option<PathBuf> {
    Some(gex_cache_root()?.join("profiles"))
}

/// Read the entry points a previous run of this image discovered.
///
/// Static discovery cannot follow a computed goto, so on a program built
/// around an interpreter loop -- SQLite's VDBE, most language runtimes -- it
/// leaves thousands of blocks to be translated on demand, every run. Recording
/// where execution actually went lets the next run translate them ahead of
/// time, which then gets compiled once and cached like everything else.
///
/// The profile is a hint, never a requirement: a stale or corrupt one costs a
/// few wasted translations, and an address that is no longer code is dropped
/// by the discovery loop's own executability check.
pub fn read_profile(dir: &Path, key: &str) -> Vec<u64> {
    let Ok(bytes) = std::fs::read(dir.join(format!("{key}.profile"))) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// Record entry points for the next run, merged with what is already there.
///
/// Merging matters because two runs of the same program exercise different
/// paths; the profile should converge on their union rather than flip between
/// them. `limit` bounds that union so a pathological program cannot grow the
/// file without end.
pub fn write_profile(dir: &Path, key: &str, found: &[u64], limit: usize) {
    let mut all: Vec<u64> = read_profile(dir, key);
    let before = all.len();
    all.extend_from_slice(found);
    all.sort_unstable();
    all.dedup();
    if all.len() == before {
        return;
    }
    all.truncate(limit);
    let mut bytes = Vec::with_capacity(all.len() * 8);
    for a in &all {
        bytes.extend_from_slice(&a.to_le_bytes());
    }
    let path = dir.join(format!("{key}.profile"));
    store(dir, &path, &bytes);
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
pub fn key<'a>(build: &str, fields: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut h = sha256::Sha256::new();
    h.update(&FORMAT_VERSION.to_le_bytes());
    h.update(&(build.len() as u64).to_le_bytes());
    h.update(build.as_bytes());
    for f in fields {
        h.update(&(f.len() as u64).to_le_bytes());
        h.update(f);
    }
    sha256::hex(&h.finish())
}

/// A compiled module, either loaded from the cache or freshly built.
pub struct Loaded {
    pub module: Module,
    /// Whatever the caller stored alongside it.
    pub metadata: Vec<u8>,
    /// True when the compile was skipped.
    pub hit: bool,
}

/// Get the compiled form of a translation, using `dir` as the cache.
///
/// `build` is called only on a miss, and returns the wasm together with
/// whatever the caller needs to remember about the translation. That is the
/// reason for the closure: translating is over a second on a large program,
/// and a hit should not pay it.
///
/// A cache that cannot be read or written is not an error: the module is
/// compiled and the run proceeds. That matters for a read-only home directory
/// and for two gex processes racing on the same entry.
pub fn load_or_compile(
    engine: &Engine,
    dir: &Path,
    key: &str,
    build: impl FnOnce() -> Result<(Vec<u8>, Vec<u8>)>,
) -> Result<Loaded> {
    let path = dir.join(format!("{key}.cwasm"));
    let meta_path = dir.join(format!("{key}.meta"));
    // Both halves must be present: a module without its metadata says nothing
    // about where its dispatch pages went.
    if let (Ok(bytes), Ok(metadata)) = (std::fs::read(&path), std::fs::read(&meta_path)) {
        // SAFETY: see the module comment. `deserialize` additionally rejects
        // an artifact that does not match this engine, so a stale or truncated
        // file produces an error rather than bad code -- which is why the
        // failure path below simply rebuilds.
        match unsafe { Module::deserialize(engine, &bytes) } {
            Ok(module) => {
                // Rewriting the metadata refreshes its modification time,
                // which is what eviction sorts by. It is a few hundred bytes
                // and it is the difference between keeping the modules that
                // get reused and keeping whichever happen to be newest.
                let _ = std::fs::write(&meta_path, &metadata);
                return Ok(Loaded {
                    module,
                    metadata,
                    hit: true,
                });
            }
            Err(_) => {
                // Unusable: drop it so the next run does not pay to find out
                // again. Failing to remove it is not worth reporting.
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(&meta_path);
            }
        }
    }

    let (wasm, metadata) = build()?;
    let module = Module::new(engine, &wasm).context("validating translated wasm")?;
    if let Ok(serialized) = module.serialize() {
        // Metadata first: a reader requires both, so the module's arrival is
        // what publishes the entry.
        store(dir, &meta_path, &metadata);
        store(dir, &path, &serialized);
        // Not on every store: the sweep lists the whole directory, and with a
        // quarter of a million entries that is not free.
        if STORES.fetch_add(1, Ordering::Relaxed) % STORES_BETWEEN_SWEEPS == 0 {
            evict(dir, budget());
        }
    }
    Ok(Loaded {
        module,
        metadata,
        hit: false,
    })
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
        let a = key("build", [b"program".as_slice(), b"opts".as_slice()]);
        assert_eq!(a, key("build", [b"program".as_slice(), b"opts".as_slice()]));
        assert_ne!(
            a,
            key("build", [b"program!".as_slice(), b"opts".as_slice()])
        );
        assert_ne!(
            a,
            key("build", [b"program".as_slice(), b"opts!".as_slice()])
        );
        assert_ne!(a, key("build", [b"program".as_slice()]));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn a_different_gex_build_is_a_different_key() {
        // The case this exists for: same program, same options, translator
        // changed underneath.
        assert_ne!(
            key("build-a", [b"program".as_slice()]),
            key("build-b", [b"program".as_slice()])
        );
    }

    #[test]
    fn the_running_build_is_identifiable() {
        assert!(super::build_fingerprint().is_some());
    }

    #[test]
    fn field_boundaries_cannot_be_shifted() {
        // Without length prefixes these two would hash the same bytes.
        assert_ne!(
            key("b", [b"ab".as_slice(), b"c".as_slice()]),
            key("b", [b"a".as_slice(), b"bc".as_slice()])
        );
    }

    #[test]
    fn a_profile_round_trips_and_merges() {
        let dir = std::env::temp_dir().join(format!("gex-prof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        assert!(read_profile(&dir, "k").is_empty());

        write_profile(&dir, "k", &[0x400, 0x100, 0x400], 100);
        assert_eq!(read_profile(&dir, "k"), vec![0x100, 0x400]);

        // A second run's addresses join the first's rather than replacing them.
        write_profile(&dir, "k", &[0x200], 100);
        assert_eq!(read_profile(&dir, "k"), vec![0x100, 0x200, 0x400]);

        // Nothing new means no rewrite, and the limit is honoured.
        write_profile(&dir, "k", &[0x100], 100);
        assert_eq!(read_profile(&dir, "k").len(), 3);
        write_profile(&dir, "k", &[0x500, 0x600], 4);
        assert_eq!(read_profile(&dir, "k").len(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_profile_is_read_as_far_as_it_goes() {
        let dir = std::env::temp_dir().join(format!("gex-prof2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Twelve bytes: one whole address and half of another.
        std::fs::write(dir.join("t.profile"), [1u8; 12]).unwrap();
        assert_eq!(read_profile(&dir, "t").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_entry_is_replaced_rather_than_fatal() {
        let dir = std::env::temp_dir().join(format!("gex-cache-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let k = "deadbeef";
        std::fs::write(dir.join(format!("{k}.cwasm")), b"not a module").unwrap();

        let engine = Engine::default();
        // The smallest valid module.
        let build = || {
            Ok((
                vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00],
                b"meta".to_vec(),
            ))
        };
        let loaded = load_or_compile(&engine, &dir, k, build).unwrap();
        assert!(!loaded.hit, "a corrupt entry must not count as a hit");
        assert_eq!(loaded.metadata, b"meta");

        // And the freshly stored one is usable, without rebuilding.
        let again = load_or_compile(&engine, &dir, k, || panic!("must not rebuild")).unwrap();
        assert!(again.hit, "the store should have replaced it");
        assert_eq!(again.metadata, b"meta");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_module_without_its_metadata_is_not_a_hit() {
        let dir = std::env::temp_dir().join(format!("gex-cache-half-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let k = "halfentry";
        let engine = Engine::default();
        let build = || {
            Ok((
                vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00],
                b"m".to_vec(),
            ))
        };
        load_or_compile(&engine, &dir, k, build).unwrap();
        std::fs::remove_file(dir.join(format!("{k}.meta"))).unwrap();

        let again = load_or_compile(&engine, &dir, k, build).unwrap();
        assert!(!again.hit, "half an entry must not be used");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    /// A cache over budget loses its least recently used entries and keeps
    /// the rest.
    #[test]
    fn eviction_keeps_what_was_read_recently() {
        let dir = std::env::temp_dir().join(format!("gex-cache-evict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Ten entries of a kilobyte each, written oldest first. The
        // modification times have to differ or the sort has nothing to go on,
        // and a filesystem's timestamp granularity can be coarse.
        for i in 0..10 {
            let key = format!("k{i}");
            std::fs::write(dir.join(format!("{key}.cwasm")), vec![0u8; 1024]).unwrap();
            std::fs::write(dir.join(format!("{key}.meta")), b"m").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        // Read the oldest, which is what a hit does.
        std::fs::write(dir.join("k0.meta"), b"m").unwrap();

        // Budget for four entries; eviction goes to 80% of it.
        evict(&dir, 4 * 1025);

        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("cwasm"))
            .map(|e| e.path().file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(left.len() <= 4, "still over budget: {left:?}");
        assert!(left.contains(&"k0".to_string()), "dropped a recent read: {left:?}");
        assert!(left.contains(&"k9".to_string()), "dropped the newest: {left:?}");
        assert!(!left.contains(&"k1".to_string()), "kept the oldest unread: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
