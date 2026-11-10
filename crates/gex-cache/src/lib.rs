//! A content-addressed cache of compiled modules.
//!
//! Compiling translated wasm to machine code is the slow part of startup
//! (Cranelift takes ~30s on a large program) and is identical on every run of
//! the same binary, so it is cached. The key hashes the guest image, the
//! translation options, and a format version. It need not cover the engine's
//! configuration: [`wasmtime::Module::deserialize`] checks that itself.
//!
//! # Safety
//!
//! Deserialising a compiled module is unsafe: the bytes become executable code
//! behind only a header check. gex trusts them because they live in a
//! per-user-writable cache directory — anyone who can write there can already
//! replace the binary that reads it. The `unsafe` lives here so the host can
//! forbid it.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use wasmtime::{Engine, Module};

/// How much disk the cache may use before evicting least-recently-read
/// entries. A limit is needed because most of what lands here is never read
/// again: a whole program hits on every later run, but a runtime-translated
/// batch keys on which blocks it held and almost never recurs.
const DEFAULT_BUDGET: u64 = 4 << 30;

/// The cache's running size lives in this file in the cache directory, so a
/// store need not list the directory (a shared cache holds hundreds of
/// thousands of entries, seconds to list and stat): each store adds its bytes,
/// each eviction subtracts what it removed, and only a sweep walks. Its text is
/// "<bytes> <unix seconds a walk last set them> <unix seconds a walk last
/// started>", each right-aligned in 20 columns, rewritten in place under a
/// lock.
const LEDGER: &str = "size";

/// The oldest entries the last walk left, as "<key> <bytes>" lines, oldest
/// last: what a store that takes the cache over budget evicts at once, with
/// no walk. Read and written only under the ledger's lock.
const VICTIMS: &str = "victims";

/// How many entries a walk leaves in [`VICTIMS`].
const VICTIMS_MAX: usize = 1024;

/// The most entries one store evicts from [`VICTIMS`].
const VICTIM_BATCH: usize = 64;

/// Held (non-blocking) by the one sweep running against a cache directory.
const SWEEP_LOCK: &str = "sweep.lock";

/// How long a ledger is trusted before a sweep re-walks the directory to
/// correct it: other gex builds share the directory and may store without
/// adding, and two processes storing one key both add it.
const RESYNC_AFTER: u64 = 3600;

/// How long after a walk started another may start, unless the cache is over
/// budget. A walk takes seconds and dies with its process, and a test run
/// starts thousands of short processes: without this, each would start a
/// walk of the whole directory and leave it unfinished.
const WALK_RETRY: u64 = 600;

/// Entries an eviction removes between ledger updates, so a process that
/// exits mid-eviction leaves a ledger that matches what it removed.
const EVICT_BATCH: usize = 256;

/// The directories this process has a sweep running against.
static SWEEPING: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Ledger {
    bytes: u64,
    /// When a walk last set `bytes`; 0 for never (a ledger stores started).
    walked: u64,
    /// When a walk last started; 0 for never.
    started: u64,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Update the ledger under its lock with `f` (given `None` when it is absent
/// or unreadable) and return what was written. `None` when the ledger cannot
/// be opened or locked.
fn ledger_update(dir: &Path, f: impl FnOnce(Option<Ledger>) -> Ledger) -> Option<Ledger> {
    use std::io::{Read, Seek, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LEDGER))
        .ok()?;
    file.lock().ok()?;
    let mut text = String::new();
    let old = file.read_to_string(&mut text).ok().and_then(|_| {
        let mut it = text.split_ascii_whitespace().map(str::parse::<u64>);
        match (it.next(), it.next(), it.next(), it.next()) {
            (Some(Ok(bytes)), Some(Ok(walked)), Some(Ok(started)), None) => Some(Ledger { bytes, walked, started }),
            _ => None,
        }
    });
    let new = f(old);
    // One length always, written over the old in one write: a process may
    // exit with its sweep thread in here, and a ledger truncated and not yet
    // rewritten would read as absent (untrusted, no walk started), so the
    // next store would start a walk of its own.
    let line = format!("{:>20} {:>20} {:>20}\n", new.bytes, new.walked, new.started);
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = file.write_all(line.as_bytes());
    // (a ledger from before was shorter)
    if text.len() > line.len() {
        let _ = file.set_len(line.len() as u64);
    }
    Some(new)
}

/// Record `delta` bytes stored (positive) or removed (negative). An absent or
/// unreadable ledger starts from the delta, marked never walked.
fn ledger_add(dir: &Path, delta: i64) -> Option<Ledger> {
    ledger_update(dir, |old| {
        let old = old.unwrap_or_default();
        Ledger { bytes: old.bytes.saturating_add_signed(delta), ..old }
    })
}

/// Evict up to `want` bytes from the oldest of the last walk's [`VICTIMS`]
/// (at most [`VICTIM_BATCH`] entries), skipping any read since that walk
/// (`walked`). Called under the ledger's lock. Returns the bytes removed.
fn evict_victims(dir: &Path, want: u64, walked: u64) -> u64 {
    let path = dir.join(VICTIMS);
    let Ok(text) = std::fs::read_to_string(&path) else { return 0 };
    let mut lines: Vec<&str> = text.lines().collect();
    let mut removed = 0u64;
    let mut n = 0;
    while removed < want && n < VICTIM_BATCH {
        let Some(line) = lines.pop() else { break };
        n += 1;
        let Some((key, size)) = line.split_once(' ') else { continue };
        let meta_path = dir.join(format!("{key}.meta"));
        // In whole seconds, as `walked` is: an entry the walk saw has an
        // mtime no later than the second the walk ended in.
        let read_since = std::fs::metadata(&meta_path)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.duration_since(std::time::UNIX_EPOCH).is_ok_and(|d| d.as_secs() > walked));
        if read_since {
            continue;
        }
        let gone = std::fs::remove_file(dir.join(format!("{key}.cwasm"))).is_ok();
        let _ = std::fs::remove_file(&meta_path);
        if gone {
            removed += size.parse::<u64>().unwrap_or(0);
        }
    }
    if lines.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let mut rest = lines.join("\n");
        rest.push('\n');
        let _ = std::fs::write(&path, rest);
    }
    removed
}

/// After a store of `added` bytes: account for it; if that takes the cache
/// over budget, evict as much from the last walk's oldest entries at once;
/// and start a sweep on a thread of its own (the store's caller is a guest
/// waiting on its code) when the cache is still over budget, or when the
/// ledger is untrusted and no walk has started for [`WALK_RETRY`]. At most one
/// sweep runs per directory, in a process and across processes. Returns the
/// sweep's thread, for tests.
fn account_store(dir: &Path, added: u64, budget: u64) -> Option<std::thread::JoinHandle<()>> {
    let now = now_secs();
    let mut start = false;
    ledger_update(dir, |old| {
        let mut l = old.unwrap_or_default();
        l.bytes = l.bytes.saturating_add(added);
        if l.bytes > budget {
            l.bytes = l.bytes.saturating_sub(evict_victims(dir, added, l.walked));
        }
        let stale = old.is_none() || l.walked == 0 || now.saturating_sub(l.walked) > RESYNC_AFTER;
        if l.bytes > budget || (stale && now.saturating_sub(l.started) >= WALK_RETRY) {
            start = true;
            l.started = now;
        }
        l
    })?;
    if !start {
        return None;
    }
    let done = |dir: &Path| {
        if let Ok(mut s) = SWEEPING.lock() {
            s.retain(|d| d != dir);
        }
    };
    {
        let mut s = SWEEPING.lock().ok()?;
        if s.iter().any(|d| d == dir) {
            return None;
        }
        s.push(dir.to_path_buf());
    }
    let owned = dir.to_path_buf();
    let spawned = std::thread::Builder::new().name("gex-cache-sweep".into()).spawn(move || {
        sweep(&owned, budget);
        done(&owned);
    });
    spawned.inspect_err(|_| done(dir)).ok()
}

/// Walk and evict, unless another process is sweeping this directory.
/// Returns the size left, or `None` when another sweep holds the directory.
fn sweep(dir: &Path, budget: u64) -> Option<u64> {
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(SWEEP_LOCK))
        .ok()?;
    lock.try_lock().ok()?;
    Some(evict(dir, budget))
}

fn budget() -> u64 {
    std::env::var("GEX_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BUDGET)
}

/// Delete least-recently-used entries until the cache is under budget.
///
/// "Least recently used" is the metadata file's mtime, which
/// [`load_or_compile`] refreshes on a hit (to the hour, [`REFRESH_AFTER`]).
/// Evicts down to 80% of budget whenever the cache is above that, not only
/// once it is over budget: a sweep that left it just under budget let the
/// next store's estimate cross it again, so every store swept the whole
/// directory. Sets the ledger to what the walk found (plus what other
/// processes stored meanwhile) and subtracts each batch it removes. Returns
/// the size left.
fn evict(dir: &Path, budget: u64) -> u64 {
    // What the ledger said before the walk: stores made during it (by any
    // process) are what it says after, less this.
    let before = ledger_update(dir, |old| old.unwrap_or_default()).map_or(0, |l| l.bytes);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    // (mtime, key, bytes) for each entry, sized as module plus metadata.
    let mut items: Vec<(std::time::SystemTime, String, u64)> = Vec::new();
    let mut walked = 0u64;
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
        walked += wasm_len + meta_len;
        items.push((when, key, wasm_len + meta_len));
    }
    let target = budget / 5 * 4;
    items.sort_by_key(|(when, _, _)| *when);
    // What this walk will evict: the oldest entries down to the target.
    let mut cut = 0;
    let mut kept = walked;
    while kept > target && cut < items.len() {
        kept -= items[cut].2;
        cut += 1;
    }
    let mut total = ledger_update(dir, |old| {
        // The next oldest, for stores to evict between walks.
        let mut victims = String::new();
        for (_, key, size) in items[cut..].iter().take(VICTIMS_MAX).rev() {
            victims.push_str(&format!("{key} {size}\n"));
        }
        let _ = std::fs::write(dir.join(VICTIMS), victims);
        let old = old.unwrap_or_default();
        Ledger {
            bytes: walked.saturating_add(old.bytes.saturating_sub(before)),
            walked: now_secs(),
            started: old.started,
        }
    })
    .map_or(walked, |l| l.bytes);
    if total <= target {
        return total;
    }
    let mut items = items.into_iter();
    while total > target {
        let mut removed = 0u64;
        for (_, key, size) in items.by_ref().take(EVICT_BATCH) {
            let _ = std::fs::remove_file(dir.join(format!("{key}.cwasm")));
            let _ = std::fs::remove_file(dir.join(format!("{key}.meta")));
            removed += size;
            if total.saturating_sub(removed) <= target {
                break;
            }
        }
        if removed == 0 {
            break;
        }
        total = ledger_add(dir, -(removed as i64)).map_or(total.saturating_sub(removed), |l| l.bytes);
    }
    total
}

/// SHA-256, from gex-key (the browser shim computes keys without wasmtime).
pub use gex_key::sha256;

/// Bumped whenever the *layout* of a cache entry changes.
///
/// What gex itself does to the guest is covered by [`build_fingerprint`], not
/// by this.
const FORMAT_VERSION: u32 = 2;

/// Identifies the running gex build, so a translator change is not served
/// stale code. Required: a decoder fix changes nothing else the key can see,
/// so without it the fixed binary reuses the broken module.
///
/// Uses the executable's size and mtime (a rebuild changes both) rather than
/// hashing tens of megabytes. `None` means the cache must not be used, since
/// an unidentifiable build cannot be told from a different one.
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

/// Where the learned 64-bit memory records live: one
/// file per program image that needed more address space than a wasm32
/// memory has, named by the sha256 of the image.
pub fn mem64_dir() -> Option<PathBuf> {
    Some(gex_cache_root()?.join("mem64"))
}

/// Where entry-point profiles live.
pub fn profile_dir() -> Option<PathBuf> {
    Some(gex_cache_root()?.join("profiles"))
}

/// Read the entry points a previous run of this image discovered.
///
/// Static discovery cannot follow a computed goto, so an interpreter-loop
/// program leaves thousands of blocks to translate on demand every run.
/// Recording where execution went lets the next run translate them ahead of
/// time. A hint, never a requirement: a stale or corrupt profile only wastes
/// a few translations.
pub fn read_profile(dir: &Path, key: &str) -> Vec<u64> {
    let Ok(bytes) = std::fs::read(dir.join(format!("{key}.profile"))) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect()
}

/// Record entry points for the next run, merged with what is already there so
/// the profile converges on the union of the paths different runs take.
/// `limit` bounds that union.
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

/// The cache key for one translation. `fields` must cover everything that
/// changes the emitted wasm (segment bytes, placement, options); the caller
/// assembles them. Each field is length-prefixed, so regrouping the same
/// bytes across fields cannot collide.
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

/// Get the compiled form of a translation, using `dir` as the cache. `build`
/// runs only on a miss, returning the wasm plus caller metadata. A cache that
/// cannot be read or written is not an error — the module is compiled and the
/// run proceeds (read-only home, or two processes racing on one entry).
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
                refresh(&meta_path);
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
                ledger_add(dir, -((bytes.len() + metadata.len()) as i64));
            }
        }
    }

    let (wasm, metadata) = build()?;
    let module = compile(engine, &wasm).context("validating translated wasm")?;
    if let Some(serialized) = serialize(&module) {
        // Metadata first: a reader requires both, so the module's arrival is
        // what publishes the entry.
        store(dir, &meta_path, &metadata);
        store(dir, &path, &serialized);
        account_store(dir, (metadata.len() + serialized.len()) as u64, budget());
    }
    Ok(Loaded {
        module,
        metadata,
        hit: false,
    })
}

/// How stale a hit's metadata mtime may be before the hit refreshes it.
/// Eviction needs no finer order than this, and a refresh is a write to the
/// cache's filesystem: a Wine child starts with some seventy hits, and when
/// each one rewrote its metadata file (a new file renamed over the old), a
/// busy disk's journal held every start for seconds.
const REFRESH_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// Mark a hit's entry as recently read for [`evict`]: its metadata file's
/// mtime set to now, in place (its bytes untouched, so a concurrent reader
/// never sees it partial), and only when older than [`REFRESH_AFTER`].
fn refresh(meta_path: &Path) {
    let Ok(f) = std::fs::File::open(meta_path) else { return };
    let now = std::time::SystemTime::now();
    let fresh = f
        .metadata()
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age < REFRESH_AFTER));
    if !fresh {
        let _ = f.set_modified(now);
    }
}

/// A compiled module kept at `path` (no eviction: for large, long-lived
/// modules such as a function-tier unit, which the shared cache's budget
/// would evict at once), compiled from `wasm` when absent or unusable.
pub fn load_or_compile_at(engine: &Engine, path: &Path, wasm: &[u8]) -> Result<Module> {
    memo(engine, wasm.len(), || format!("at:{}", path.display()), || load_or_compile_at_uncached(engine, path, wasm))
}

/// Modules at least this large are kept for the life of the host process
/// and handed to every later request for the same bytes: a module is
/// immutable code and every store of one engine can instantiate it, so a
/// guest process that starts the same program or library as an earlier one
/// (every Wine child starts the same 5.7 MB loader; 2 s of Cranelift each)
/// takes the compiled one. Small modules (runtime translations, by the
/// hundred thousand, rarely repeated) are not kept.
const MEMO_MIN_BYTES: usize = 1 << 20;

/// The compiled modules kept by [`MEMO_MIN_BYTES`]: (engine, key, module).
#[allow(clippy::type_complexity)]
static MEMO: std::sync::Mutex<Vec<(Engine, String, Module)>> = std::sync::Mutex::new(Vec::new());

fn memo(engine: &Engine, len: usize, key: impl FnOnce() -> String, make: impl FnOnce() -> Result<Module>) -> Result<Module> {
    if len < MEMO_MIN_BYTES || std::env::var_os("GEX_NO_MODULE_MEMO").is_some() {
        return make();
    }
    let key = key();
    if let Some((_, _, m)) = MEMO.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(e, k, _)| k == &key && Engine::same(e, engine)) {
        return Ok(m.clone());
    }
    // Compiled outside the lock: another thread may compile the same module
    // meanwhile, and both results are good; the first one kept wins.
    let m = make()?;
    let mut g = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, _, kept)) = g.iter().find(|(e, k, _)| k == &key && Engine::same(e, engine)) {
        return Ok(kept.clone());
    }
    g.push((engine.clone(), key, m.clone()));
    Ok(m)
}

fn load_or_compile_at_uncached(engine: &Engine, path: &Path, wasm: &[u8]) -> Result<Module> {
    if let Ok(bytes) = std::fs::read(path) {
        // SAFETY: as for `load_or_compile`: written by `serialize` below, and
        // `deserialize` rejects an artifact for another engine.
        match unsafe { Module::deserialize(engine, &bytes) } {
            Ok(m) => return Ok(m),
            Err(_) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let m = compile(engine, wasm).context("validating the module")?;
    if let (Some(bytes), Some(dir)) = (serialize(&m), path.parent()) {
        store(dir, path, &bytes);
    }
    Ok(m)
}

/// A compiled module's artifact, when this build can write one.
#[cfg(feature = "compile")]
fn serialize(m: &Module) -> Option<Vec<u8>> {
    m.serialize().ok()
}

/// [`key`], remembered for the host process when the fields come to
/// [`MEMO_MIN_BYTES`] or more, by a 128-bit SipHash of the same
/// length-prefixed stream: under 32-bit Wine every child execs the same 5.7 MB
/// loader and installs the same 8.8 MB PE module, and SHA-256 over them was
/// about a tenth of a session boot's host CPU.
pub fn key_memo<'a>(build: &str, fields: impl IntoIterator<Item = &'a [u8]> + Clone) -> String {
    use std::hash::Hasher;
    let total: usize = fields.clone().into_iter().map(|f| f.len()).sum();
    if total < MEMO_MIN_BYTES {
        return key(build, fields);
    }
    static KEYS: std::sync::Mutex<Vec<((u64, u64), String)>> = std::sync::Mutex::new(Vec::new());
    let mut hs = [std::collections::hash_map::DefaultHasher::new(), std::collections::hash_map::DefaultHasher::new()];
    hs[1].write_u64(0x9e37_79b9_7f4a_7c15);
    for h in hs.iter_mut() {
        h.write(build.as_bytes());
        for f in fields.clone() {
            h.write_u64(f.len() as u64);
            h.write(f);
        }
    }
    let id = (hs[0].finish(), hs[1].finish());
    if let Some((_, k)) = KEYS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(i, _)| *i == id) {
        return k.clone();
    }
    let k = key(build, fields);
    let mut m = KEYS.lock().unwrap_or_else(|e| e.into_inner());
    if m.len() >= 64 {
        m.remove(0);
    }
    m.push((id, k.clone()));
    k
}

#[cfg(not(feature = "compile"))]
fn serialize(_m: &Module) -> Option<Vec<u8>> {
    None
}

/// Where modules compiled ahead of time are kept by content
/// (`GEX_PRECOMPILED`, an ejected project's): `<dir>/<sha256 of the wasm>.cwasm`.
pub fn precompiled_dir() -> Option<PathBuf> {
    std::env::var_os("GEX_PRECOMPILED").map(PathBuf::from)
}

fn content_name(wasm: &[u8]) -> String {
    // (the same SHA-256, remembered for a large module: see `key_memo`)
    let id = (wasm.len() >= MEMO_MIN_BYTES).then(|| sip128(wasm));
    if let Some(id) = id {
        if let Some((_, k)) = KEYS_CONTENT.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(i, _)| *i == id) {
            return k.clone();
        }
    }
    let mut h = sha256::Sha256::new();
    h.update(wasm);
    let name = format!("{}.cwasm", sha256::hex(&h.finish()));
    if let Some(id) = id {
        let mut m = KEYS_CONTENT.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() >= 64 {
            m.remove(0);
        }
        m.push((id, name.clone()));
    }
    name
}

static KEYS_CONTENT: std::sync::Mutex<Vec<((u64, u64), String)>> = std::sync::Mutex::new(Vec::new());

fn sip128(bytes: &[u8]) -> (u64, u64) {
    use std::hash::Hasher;
    let mut a = std::collections::hash_map::DefaultHasher::new();
    let mut b = std::collections::hash_map::DefaultHasher::new();
    b.write_u64(0x9e37_79b9_7f4a_7c15);
    a.write(bytes);
    b.write(bytes);
    (a.finish(), b.finish())
}

/// A module for `wasm`: compiled ahead of time and found by its content in
/// [`precompiled_dir`], else compiled here. A runtime built without a
/// compiler (no `compile` feature: an ejected project's) can only do the
/// first, and says so when it cannot.
pub fn compile(engine: &Engine, wasm: &[u8]) -> Result<Module> {
    memo(engine, wasm.len(), || content_name(wasm), || compile_uncached(engine, wasm))
}

fn compile_uncached(engine: &Engine, wasm: &[u8]) -> Result<Module> {
    if let Some(dir) = precompiled_dir() {
        if let Ok(bytes) = std::fs::read(dir.join(content_name(wasm))) {
            // SAFETY: written by `precompile` with this runtime's engine
            // configuration; `deserialize` rejects any other engine's.
            if let Ok(m) = unsafe { Module::deserialize(engine, &bytes) } {
                return Ok(m);
            }
        }
    }
    compile_here(engine, wasm)
}

/// [`compile`], keeping what it compiles: a native wasm32 program (a
/// gexcc build) and the libraries it links are whole modules compiled at
/// every exec, seconds for a large one, so their compiled forms go in the
/// shared cache as [`load_or_compile_module`]'s do (under the cache's
/// budget: they once went to `cache_dir()/wasm32`, which nothing evicted).
pub fn compile_cached(engine: &Engine, wasm: &[u8]) -> Result<Module> {
    load_or_compile_module(engine, wasm)
}

/// A whole module loaded as it is (Wine's native PE modules and their core,
/// a library provider), compiled once and kept in the shared cache
/// ([`cache_dir`], under its budget and eviction, stored by temp file and
/// rename) by its content and the engine's configuration
/// ([`module_key`]): never beside the module's file, which may be another
/// install's, read through a link. With [`precompiled_dir`] set (an ejected
/// project), or without a compiler, as [`compile`].
pub fn load_or_compile_module(engine: &Engine, wasm: &[u8]) -> Result<Module> {
    let (Some(dir), Some(key)) = (cache_dir().filter(|_| precompiled_dir().is_none()), module_key(engine, wasm)) else {
        return compile(engine, wasm);
    };
    memo(engine, wasm.len(), || format!("mod:{key}"), || {
        Ok(load_or_compile(engine, &dir, &key, || Ok((wasm.to_vec(), Vec::new())))?.module)
    })
}

/// [`load_or_compile_module`]'s key: the module's bytes and the engine's
/// precompile compatibility (what `deserialize` checks), so engines of two
/// configurations (a 64-bit memory's, a wasm32 one's) keep an entry each
/// rather than recompiling over each other's. None without a compiler.
#[cfg(feature = "compile")]
pub fn module_key(engine: &Engine, wasm: &[u8]) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    engine.precompile_compatibility_hash().hash(&mut h);
    let e = h.finish().to_le_bytes();
    Some(key_memo("module", [wasm, &e[..]]))
}

#[cfg(not(feature = "compile"))]
pub fn module_key(_engine: &Engine, _wasm: &[u8]) -> Option<String> {
    None
}

#[cfg(feature = "compile")]
fn compile_here(engine: &Engine, wasm: &[u8]) -> Result<Module> {
    // `GEX_COMPILE_TRACE`: every compile here, its size and time, and who
    // asked (the first few frames of ours).
    let t0 = std::time::Instant::now();
    let m = Module::new(engine, wasm).map_err(anyhow::Error::from);
    if std::env::var_os("GEX_COMPILE_TRACE").is_some() {
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let who: Vec<&str> = bt
            .lines()
            .map(str::trim)
            .filter(|l| l.contains(": gex_") && !l.contains("gex_cache::"))
            .map(|l| l.split_once(": ").map(|x| x.1).unwrap_or(l))
            .take(4)
            .collect();
        eprintln!("gex: compiled {} KiB in {:.3}s for {}", wasm.len() >> 10, t0.elapsed().as_secs_f64(), who.join(" < "));
    }
    m
}

#[cfg(not(feature = "compile"))]
fn compile_here(_engine: &Engine, wasm: &[u8]) -> Result<Module> {
    anyhow::bail!(
        "a {}-byte wasm module was not compiled ahead of time ({} in {}), and this runtime has no compiler",
        wasm.len(),
        content_name(wasm),
        precompiled_dir().map(|d| d.display().to_string()).unwrap_or_else(|| "GEX_PRECOMPILED (unset)".into())
    )
}

/// Compile `wasm` into `dir` by content, for a runtime without a compiler
/// ([`compile`]).
#[cfg(feature = "compile")]
pub fn precompile(engine: &Engine, wasm: &[u8], dir: &Path) -> Result<()> {
    let path = dir.join(content_name(wasm));
    if path.exists() {
        return Ok(());
    }
    let m = Module::new(engine, wasm).map_err(anyhow::Error::from)?;
    std::fs::create_dir_all(dir)?;
    store(dir, &path, &m.serialize().map_err(anyhow::Error::from)?);
    Ok(())
}

/// Write an artifact, atomically enough that a concurrent reader never sees a
/// partial file: write to a unique name in the same directory, then rename.
fn store(dir: &Path, path: &Path, bytes: &[u8]) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Unique per *write*, not per process: two threads storing the same key
    // would otherwise share one temp file and one would rename the other's
    // half-written bytes into place.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = dir.join(format!(
        "tmp-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        path.file_name().and_then(|n| n.to_str()).unwrap_or("x")
    ));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    /// A hit must never leave the metadata visible in a partial state:
    /// refreshing the mtime used to truncate-and-rewrite in place and race a
    /// concurrent reader.
    #[test]
    fn a_concurrent_reader_never_sees_half_a_meta_file() {
        let dir = std::env::temp_dir().join(format!("gex-meta-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.meta");
        let payload = vec![7u8; 4096];
        super::store(&dir, &path, &payload);

        let stop = std::sync::atomic::AtomicBool::new(false);
        let saw_partial = std::sync::atomic::AtomicUsize::new(usize::MAX);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        super::store(&dir, &path, &payload);
                    }
                });
            }
            for _ in 0..2000 {
                // Absent is allowed -- a reader can look between the unlink
                // and the link of a rename. Present and the wrong length is
                // not.
                if let Ok(got) = std::fs::read(&path) {
                    if got.len() != payload.len() {
                        saw_partial.store(got.len(), std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }
            // Set before any assertion: a panic here would leave the writers
            // spinning and the scope never joining, so a failing test would
            // hang instead of failing.
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let short = saw_partial.load(std::sync::atomic::Ordering::Relaxed);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            short,
            usize::MAX,
            "a reader saw a {short}-byte file where {} were written",
            payload.len()
        );
    }

    use super::*;

    /// A hit writes nothing to the cache unless its entry's mtime is stale,
    /// and then only the mtime: the same file (no new one renamed over it),
    /// its bytes untouched.
    #[test]
    fn a_hit_refreshes_a_stale_mtime_in_place_and_leaves_a_fresh_one() {
        use std::os::unix::fs::MetadataExt;
        let dir = std::env::temp_dir().join(format!("gex-meta-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("k.meta");
        std::fs::write(&path, b"meta").unwrap();
        let ino = std::fs::metadata(&path).unwrap().ino();
        let set = |ago: u64| {
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(ago);
            std::fs::File::open(&path).unwrap().set_modified(t).unwrap();
            t
        };
        let mtime = || std::fs::metadata(&path).unwrap().modified().unwrap();

        let recent = set(600);
        refresh(&path);
        assert_eq!(mtime(), recent, "a mtime newer than REFRESH_AFTER is left");

        let stale = set(2 * 3600);
        refresh(&path);
        let after = mtime();
        assert!(after > stale + std::time::Duration::from_secs(3600), "a stale mtime is refreshed");
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), ino, "in place, not a new file");
        assert_eq!(std::fs::read(&path).unwrap(), b"meta");
        assert!(std::fs::read_dir(&dir).unwrap().count() == 1, "no temporary file left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With the mtime refreshed at most once an hour, an entry read all the
    /// time still sorts as recent: its mtime is within the hour, and entries
    /// not read for longer go before it.
    #[test]
    fn eviction_keeps_an_entry_read_all_the_time() {
        let dir = std::env::temp_dir().join(format!("gex-evict-hour-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now();
        // Read 59 minutes ago and refreshed then, read since (no refresh);
        // last read 2 h and 3 h ago.
        for (k, ago) in [("busy", 59 * 60), ("old", 2 * 3600), ("older", 3 * 3600)] {
            std::fs::write(dir.join(format!("{k}.cwasm")), [0u8; 1000]).unwrap();
            let meta = dir.join(format!("{k}.meta"));
            std::fs::write(&meta, b"m").unwrap();
            std::fs::File::open(&meta).unwrap().set_modified(now - std::time::Duration::from_secs(ago)).unwrap();
        }
        refresh(&dir.join("busy.meta"));
        // Over budget; eviction goes down to 80% of it, room for one entry
        // (1001 bytes): two must go.
        let left = evict(&dir, 1300);
        let kept: Vec<bool> = ["busy", "old", "older"].iter().map(|k| dir.join(format!("{k}.cwasm")).exists()).collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(kept, [true, false, false], "the busy entry is kept, the two idle ones evicted");
        assert_eq!(left, 1001);
    }

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

    /// A sweep that finds the cache under budget but above 80% of it still
    /// takes it down to 80%, so the stores after it do not sweep again.
    #[test]
    fn a_sweep_near_the_budget_leaves_room() {
        let dir = std::env::temp_dir().join(format!("gex-cache-near-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..10 {
            std::fs::write(dir.join(format!("k{i}.cwasm")), vec![0u8; 1024]).unwrap();
            std::fs::write(dir.join(format!("k{i}.meta")), b"m").unwrap();
        }
        // 10 entries of 1025 bytes against a budget of 10.5 of them: under it.
        let budget = 10 * 1025 + 512;
        let left = evict(&dir, budget);
        assert!(left <= budget / 5 * 4, "left {left} bytes, more than 80% of {budget}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn entry(dir: &Path, i: usize) {
        std::fs::write(dir.join(format!("k{i}.cwasm")), vec![0u8; 1024]).unwrap();
        std::fs::write(dir.join(format!("k{i}.meta")), b"m").unwrap();
    }

    fn on_disk(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| matches!(e.path().extension().and_then(|x| x.to_str()), Some("cwasm" | "meta")))
            .map(|e| e.metadata().unwrap().len())
            .sum()
    }

    /// Stores keep the ledger without listing the directory; a sweep
    /// corrects it to what is there; a store that takes the cache over
    /// budget evicts its own bytes' worth of the oldest entries at once,
    /// without a walk.
    #[test]
    fn the_ledger_follows_stores_and_eviction() {
        let dir = std::env::temp_dir().join(format!("gex-cache-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let budget = 8 * 1025;
        let read = |dir: &Path| ledger_update(dir, |old| old.unwrap()).unwrap();

        // No ledger yet: the first store starts one, never walked, and so
        // starts a sweep, which finds what is really there.
        for i in 0..4 {
            entry(&dir, i);
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        account_store(&dir, 1025, budget).expect("a sweep").join().unwrap();
        let l = read(&dir);
        assert_eq!(l.bytes, 4 * 1025);
        assert_ne!(l.walked, 0);
        assert_ne!(l.started, 0);

        // Stores up to the budget only add.
        for i in 4..8 {
            entry(&dir, i);
            assert!(account_store(&dir, 1025, budget).is_none(), "swept under budget");
        }
        assert_eq!(read(&dir).bytes, 8 * 1025);

        // One more crosses it: the oldest entry goes at once, and no walk.
        entry(&dir, 8);
        assert!(account_store(&dir, 1025, budget).is_none(), "walked with victims to hand");
        assert!(!dir.join("k0.cwasm").exists(), "the oldest was kept");
        assert!(dir.join("k1.cwasm").exists(), "evicted more than the store added");
        assert_eq!(read(&dir).bytes, on_disk(&dir));
        assert!(read(&dir).bytes <= budget);

        // A ledger that cannot be trusted is corrected by a walk, which
        // also takes the cache to 80%.
        ledger_update(&dir, |old| Ledger { bytes: 1, walked: 1, ..old.unwrap() });
        let left = sweep(&dir, budget).unwrap();
        assert_eq!(left, on_disk(&dir));
        assert!(left <= budget / 5 * 4, "{left} bytes left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Short processes in a row (a test run's) start at most one walk among
    /// them, though each exits with its walk unfinished and the ledger still
    /// untrusted.
    #[test]
    fn short_processes_start_one_walk() {
        let dir = std::env::temp_dir().join(format!("gex-cache-short-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..4 {
            entry(&dir, i);
        }
        let exe = std::env::current_exe().unwrap();
        let mut walks = 0;
        for _ in 0..20 {
            let out = std::process::Command::new(&exe)
                .args(["--ignored", "--exact", "budget_tests::a_short_process_stores", "--nocapture", "--test-threads=1"])
                .env("GEX_CACHE_SHORT_DIR", &dir)
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains("stored"), "child did not run: {text}");
            walks += text.matches("walk started").count();
        }
        assert_eq!(walks, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One short process for [`short_processes_start_one_walk`]: a store,
    /// then exit at once, killing any walk it started.
    #[test]
    #[ignore]
    fn a_short_process_stores() {
        let Some(dir) = std::env::var_os("GEX_CACHE_SHORT_DIR") else { return };
        let walk = account_store(Path::new(&dir), 1, u64::MAX).is_some();
        println!("stored{}", if walk { ", walk started" } else { "" });
        std::process::exit(0);
    }

    /// A sweep running in one process keeps another from starting one.
    #[test]
    fn one_sweep_at_a_time_per_directory() {
        let dir = std::env::temp_dir().join(format!("gex-cache-onesweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let held = std::fs::File::create(dir.join(SWEEP_LOCK)).unwrap();
        held.lock().unwrap();
        assert_eq!(sweep(&dir, 1 << 20), None);
        held.unlock().unwrap();
        assert_eq!(sweep(&dir, 1 << 20), Some(0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
