//! The function tier for the NT layer's images (S3, M7; docs/function-tier.md
//! §11).
//!
//! Wine's loader maps every image itself, so a Windows program's code
//! arrives through the runtime translator. With `gexc run --cgen` on a PE
//! program, every image the NT layer maps (the hook in
//! `gex_kernel::nt::section`) gets its function-tier unit, if one was built
//! for exactly those bytes at that base ([`prepare_pe`], `gexc cgen-image`):
//! the whole image's recovered functions as C, linked once, spliced over
//! tier-1 stubs at their entries against this process's dispatch slots, and
//! installed like a runtime translation. Everything else -- code recovery
//! missed, code generated at run time -- stays the runtime translator's.
//!
//! A unit is keyed by the image as placed (its relocated bytes at its base)
//! and the build of gex: [`image_key`]. Before one is installed, the image's
//! executable bytes in guest memory are compared with what the unit was built
//! from; a mismatch stops the run, naming the first differing address. An
//! image without a unit runs on tier-1, said under `GEX_TRACE`; the program's
//! own exe without one stops the run, since `--cgen` was asked for.

use super::*;
use std::collections::HashMap;
use std::sync::OnceLock;

/// An image the NT layer mapped, waiting for the host.
struct Pending {
    path: String,
    base: u64,
    pe: Arc<gex_ld::pe::PeImage>,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
static ON: OnceLock<()> = OnceLock::new();
/// One unit is placed at a time: slot assignment runs from a snapshot of the
/// page table to the publish.
static PLACING: Mutex<()> = Mutex::new(());

fn trace() -> bool {
    std::env::var_os("GEX_TRACE").is_some() || std::env::var_os("GEX_NT_CGEN_TRACE").is_some()
}

/// Where units live: `GEX_CGEN_CACHE`, else `<gex cache>/cgen`.
pub fn cache_root() -> Result<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("GEX_CGEN_CACHE") {
        return Ok(d.into());
    }
    let modules = gex_cache::cache_dir().ok_or_else(|| anyhow!("no cache directory (set GEX_CGEN_CACHE)"))?;
    Ok(modules.parent().map(|p| p.join("cgen")).unwrap_or(modules))
}

/// The identity of a unit: the format, the sources of everything that
/// decides its contents (`gex_cgen::image::INPUTS`: recovery, the lifters,
/// the IR, the C), and the image as placed (`aot::image_identity`: its
/// relocated segments at its base). clang's version and flags key each
/// object inside it.
pub fn image_key(image: &gex_ld::Image) -> String {
    let mut h = gex_cache::sha256::Sha256::new();
    h.update(gex_cgen::image::FORMAT.as_bytes());
    h.update(gex_cgen::image::INPUTS.as_bytes());
    h.update(aot::image_identity(image).as_bytes());
    gex_cache::sha256::hex(&h.finish())
}

/// Build (or find) the unit's C module for a PE image placed at `base`:
/// recovery once (cached), then C compiled unit by unit. Returns its path.
pub fn prepare_pe(pe: &gex_ld::pe::PeImage, base: u64, jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let (image, seeds) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    let key = image_key(&image);
    let root = cache_root()?;
    let dir = root.join(&key);
    let out = dir.join("c.wasm");
    if out.exists() {
        return Ok(out);
    }
    let recovery = dir.join("recovery");
    if !recovery.join("summaries").exists() {
        let t0 = std::time::Instant::now();
        let prog = gex_xlate::func::recover_pe(&image, &seeds, &Default::default());
        let summaries = gex_xlate::summary::analyze(&prog).map;
        let n = prog.functions.len();
        let funcs = gex_cgen::select(
            prog.functions.into_values().collect(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
        )?;
        gex_cgen::image::write_recovery(&recovery, &funcs, &summaries, image.class32)?;
        if verbose {
            eprintln!(
                "[cgen] recovered {n} functions ({} kept) in {:.1}s",
                funcs.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    let t0 = std::time::Instant::now();
    let work = dir.join("work");
    let stats = gex_cgen::image::build_c_module(&recovery, &work, &root.join("objects"), &out, jobs)?;
    let _ = std::fs::remove_dir_all(&work);
    if verbose {
        eprintln!(
            "[cgen] {} functions, {} units ({} compiled, {} cached), {} MiB of C, {} MiB wasm, {:.1}s",
            stats.functions,
            stats.units,
            stats.compiled,
            stats.cached,
            stats.c_bytes >> 20,
            stats.wasm_bytes >> 20,
            t0.elapsed().as_secs_f64()
        );
    }
    Ok(out)
}

/// Turn the function tier on for this process's images (`--cgen`).
pub(crate) fn enable() -> Result<()> {
    if ON.set(()).is_err() {
        return Ok(());
    }
    gex_kernel::nt::section::set_image_mapped_hook(|m| {
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).push(Pending {
            path: m.path.to_string(),
            base: m.base,
            pe: Arc::clone(m.pe),
        });
    });
    Ok(())
}

fn take_pending() -> Vec<Pending> {
    std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// A unit for `p`, if one was built: the byte check, then the stubs and the
/// splice against `page_table`.
fn place(
    p: &Pending,
    memory: &[u8],
    exec: &[(u64, u64)],
    known: &std::collections::HashSet<u64>,
    page_table: gex_xlate::PageTable,
    limits: gex_xlate::BlockLimits,
) -> Result<Option<(gex_xlate::TranslatedModule, gex_xlate::Stats)>> {
    let (image, _) = gex_xlate::func::pe_input(&p.pe, p.base)
        .map_err(|e| anyhow!("{}: placing the image at {:#x}: {e}", p.path, p.base))?;
    let cwasm_path = cache_root()?.join(image_key(&image)).join("c.wasm");
    let Ok(cwasm) = std::fs::read(&cwasm_path) else {
        return Ok(None);
    };
    // What the unit was built from must be what is mapped.
    for seg in image.segments.iter().filter(|s| s.executable) {
        let (lo, n) = (seg.vaddr as usize, seg.bytes.len());
        let live = memory
            .get(lo..lo + n)
            .ok_or_else(|| anyhow!("{}: {:#x}+{n:#x} is outside guest memory", p.path, seg.vaddr))?;
        if let Some(i) = (0..n).find(|&i| live[i] != seg.bytes[i]) {
            bail!(
                "{}: the mapped code at {:#x} differs from the image its function-tier unit was \
                 built from (guest {:#04x}, unit {:#04x}); refusing the unit",
                p.path,
                seg.vaddr + i as u64,
                live[i],
                seg.bytes[i]
            );
        }
    }
    let executable = |a: u64| exec.iter().any(|(s, e)| a >= *s && a + gex_abi::INST_ALIGN <= *e);
    let (module, stats) = gex_xlate::translate_runtime_prebuilt(
        memory,
        &executable,
        &|a| known.contains(&a),
        page_table,
        limits,
        true,
        &cwasm,
    )
    .map_err(|e| anyhow!("{}: the function tier: {e}", p.path))?;
    Ok(Some((module, stats)))
}

/// Compile a placed unit (through the module cache: the same image placed
/// in the same order compiles once).
fn compile(engine: &Engine, cache: Option<&(std::path::PathBuf, String)>, tm: &gex_xlate::TranslatedModule) -> Result<Module> {
    Ok(match cache {
        Some((dir, build)) => {
            let key = gex_cache::key(build, [tm.wasm.as_slice()]);
            let wasm = tm.wasm.clone();
            gex_cache::load_or_compile(engine, dir, &key, move || Ok((wasm, Vec::new())))?.module
        }
        None => Module::new(engine, &tm.wasm)?,
    })
}

/// Images waiting for their unit to be built, and the one being built.
/// Builds run one at a time, in a child `gexc cgen-image` (under a memory
/// cap when `systemd-run` is there: `GEX_CGEN_MEMORY_MAX`, default 12G), so a
/// recovery's peak is never the running program's and two never overlap.
static QUEUE: Mutex<std::collections::VecDeque<Pending>> = Mutex::new(std::collections::VecDeque::new());
static RUNNING: Mutex<Option<(Pending, std::process::Child)>> = Mutex::new(None);

fn spawn_build(p: &Pending) -> Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let max = std::env::var("GEX_CGEN_MEMORY_MAX").unwrap_or_else(|_| "12G".into());
    let base = format!("{:#x}", p.base);
    let args = ["cgen-image", p.path.as_str(), "--pe-base", base.as_str()];
    let capped = std::process::Command::new("systemd-run")
        .args(["--user", "--scope", "-q", "-p", &format!("MemoryMax={max}")])
        .arg("nice")
        .arg("-n")
        .arg("10")
        .arg(&exe)
        .args(args)
        .stdin(std::process::Stdio::null())
        .spawn();
    match capped {
        Ok(c) => Ok(c),
        Err(_) => Ok(std::process::Command::new(&exe).args(args).stdin(std::process::Stdio::null()).spawn()?),
    }
}

/// With builds off (`GEX_NT_CGEN_BUILD=0`): the exe without a unit stops the
/// run (`--cgen` was asked for); a DLL runs on tier-1, said under GEX_TRACE.
fn missing(p: &Pending) -> Result<()> {
    let is_exe = p.pe.characteristics & 0x2000 == 0; // IMAGE_FILE_DLL
    if is_exe {
        bail!(
            "--cgen: {} has no function-tier unit for base {:#x}; build one with \
             `gexc cgen-image {} --pe-base {:#x}`",
            p.path,
            p.base,
            p.path,
            p.base
        );
    }
    if trace() {
        eprintln!("gex-nt: {} at {:#x}: no function-tier unit, tier-1", p.path, p.base);
    }
    Ok(())
}

/// Queue a unit build for `p` (`--cgen` with no unit for that base yet). The
/// image runs on tier-1 meanwhile and gets the unit once it is built.
fn queue_build(p: Pending) -> Result<()> {
    // `GEX_NT_CGEN_BUILD`: unset or 1 builds every image; 0 none; a comma
    // list of file names (any case) only those.
    match std::env::var("GEX_NT_CGEN_BUILD").as_deref() {
        Err(_) | Ok("1") => {}
        Ok("0") => return missing(&p),
        Ok(list) => {
            let name = p.path.rsplit(['/', '\\']).next().unwrap_or(&p.path);
            if !list.split(',').any(|n| n.trim().eq_ignore_ascii_case(name)) {
                return missing(&p);
            }
        }
    }
    eprintln!("gex-nt: no function-tier unit for {} at {:#x}: building it (tier-1 until then)", p.path, p.base);
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push_back(p);
    poll_builds();
    Ok(())
}

/// Start the next build if none is running; return images whose build
/// finished, to be placed.
fn poll_builds() -> Vec<Pending> {
    let mut done = Vec::new();
    let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, child)) = running.as_mut() {
        match child.try_wait() {
            Ok(None) => return done,
            Ok(Some(status)) => {
                let (p, _) = running.take().unwrap();
                if status.success() {
                    done.push(p);
                } else {
                    eprintln!("gex-nt: building the function tier for {} failed ({status}); it stays tier-1", p.path);
                }
            }
            Err(e) => {
                let (p, _) = running.take().unwrap();
                eprintln!("gex-nt: building the function tier for {}: {e}", p.path);
            }
        }
    }
    let next = QUEUE.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
    if let Some(p) = next {
        match spawn_build(&p) {
            Ok(c) => *running = Some((p, c)),
            Err(e) => eprintln!("gex-nt: could not start building the function tier for {}: {e}", p.path),
        }
    }
    done
}

/// Place, compile, publish and install, for a caller inside the guest
/// (after an NT syscall that mapped an image).
pub(crate) fn drain_in_caller(caller: &mut Caller<'_, HostState>) -> Result<()> {
    if ON.get().is_none() {
        return Ok(());
    }
    let mut pending = take_pending();
    pending.extend(poll_builds().into_iter().filter(|p| still_mapped(caller.data_mut(), p)));
    if pending.is_empty() {
        return Ok(());
    }
    let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        let (shared, engine, limits, cache) = {
            let jit = caller.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.limits, jit.cache.clone())
        };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let exec = exec_ranges(caller.data_mut());
        let (page_table, known) = {
            let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            (sh.page_table.clone(), sh.translated.clone())
        };
        let t0 = std::time::Instant::now();
        let Some((tm, stats)) = place(&p, memory.bytes(), &exec, &known, page_table, limits)? else {
            queue_build(p)?;
            continue;
        };
        let module = compile(&engine, cache.as_ref(), &tm).with_context(|| format!("compiling {}'s function tier", p.path))?;
        publish_unit(&shared, &tm, module, stats.blocks)?;
        replay_registry(caller)?;
        report(&p, &tm, &stats, t0);
    }
    Ok(())
}

/// The same, at boot (the exe and ntdll, mapped before any guest code runs).
pub(crate) fn drain_in_store(store: &mut Store<HostState>) -> Result<()> {
    let pending = take_pending();
    if pending.is_empty() {
        return Ok(());
    }
    let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        let (shared, engine, limits, cache) = {
            let jit = store.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.limits, jit.cache.clone())
        };
        let memory = store.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let exec = exec_ranges(store.data_mut());
        let (page_table, known) = {
            let sh = shared.lock().unwrap_or_else(|e| e.into_inner());
            (sh.page_table.clone(), sh.translated.clone())
        };
        let t0 = std::time::Instant::now();
        let Some((tm, stats)) = place(&p, memory.bytes(), &exec, &known, page_table, limits)? else {
            queue_build(p)?;
            continue;
        };
        let module = compile(&engine, cache.as_ref(), &tm).with_context(|| format!("compiling {}'s function tier", p.path))?;
        let unit = publish_unit(&shared, &tm, module, stats.blocks)?;
        install_unit(&mut *store, &unit)?;
        store.data_mut().jit.as_mut().unwrap().replayed += 1;
        report(&p, &tm, &stats, t0);
    }
    Ok(())
}

fn report(p: &Pending, tm: &gex_xlate::TranslatedModule, stats: &gex_xlate::Stats, t0: std::time::Instant) {
    eprintln!(
        "gex-nt: function tier for {} at {:#x}: {} entries, {} MiB wasm, placed in {:.1}s",
        p.path,
        p.base,
        stats.blocks,
        tm.wasm.len() >> 20,
        t0.elapsed().as_secs_f64()
    );
}

/// A finished build's image is still where it was mapped: some executable
/// mapping still starts at its base. (One that was unmapped meanwhile is
/// skipped, said; one replaced by different bytes fails the byte check.)
fn still_mapped(state: &mut HostState, p: &Pending) -> bool {
    let end = p.base + p.pe.image_size as u64;
    let here = state.with_kernel(|k| k.mem.vmas().iter().any(|v| v.start < end && p.base < v.end && v.prot & prot::EXEC != 0));
    if !here {
        eprintln!("gex-nt: {} is no longer mapped at {:#x}; its new unit is not installed", p.path, p.base);
    }
    here
}

fn exec_ranges(state: &mut HostState) -> Vec<(u64, u64)> {
    state.with_kernel(|k| {
        k.mem
            .vmas()
            .iter()
            .filter(|v| v.prot & prot::EXEC != 0)
            .map(|v| (v.start, v.end))
            .collect()
    })
}
