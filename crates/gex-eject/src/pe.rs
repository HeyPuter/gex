//! Windows programs (docs/eject-pe.md): a project ejected from a session of
//! a PE program on gex's NT layer builds every image the session mapped as
//! a whole-image function-tier unit at its recorded base, pins those bases
//! with an exact layout, and runs with no run-time translation at all
//! (`gex_native::aot_only`).
//!
//! The session's `nt` section says how the NT layer was set up; the rootfs
//! holds the images and the files the NT layer opened, at their guest paths.

use crate::build::{copy_tree, verify_rootfs, BuildManifest, BuiltImage, BUILD_FILE};
use crate::config::Config;
use crate::project::{rootfs_rel, Project};
use anyhow::{anyhow, bail, Context, Result};
use gex_kernel::nt;
use gex_native::nt_cgen;
use gex_native::{Exit, RunOptions};
use gex_xlate::profile::{NtSession, Session};
use std::path::{Path, PathBuf};

/// The recorded session of a project, when it was a Windows program's.
pub fn nt_session(dir: &Path) -> Result<Option<NtSession>> {
    let p = dir.join("session/session.json");
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok(Session::from_json(&text).map_err(|e| anyhow!("{}: {e}", p.display()))?.nt)
}

/// Where the project's units are: the runtime's `GEX_CGEN_CACHE`.
pub fn units_dir(dir: &Path) -> PathBuf {
    dir.join("build/units")
}

fn abi_of(nt: &NtSession) -> Result<nt::Abi> {
    match nt.abi.as_str() {
        "p32" => Ok(nt::Abi::P32),
        "p64" => Ok(nt::Abi::P64),
        a => bail!("session.json: unknown NT ABI {a:?}"),
    }
}

/// The images to build: each (guest path, base) once, in mapping order.
fn images(nt: &NtSession) -> Vec<&gex_xlate::profile::NtImage> {
    let mut v: Vec<&gex_xlate::profile::NtImage> = Vec::new();
    for m in &nt.images {
        if !v.iter().any(|x| x.path == m.path && x.base == m.base) {
            v.push(m);
        }
    }
    v
}

/// What a PE build made.
#[derive(Debug, Default)]
pub struct PeReport {
    pub units: Vec<(String, u64, usize, u64, &'static str)>,
    pub providers: Vec<String>,
    /// Addresses the session ran on tier-1 that no unit has an entry for,
    /// by image: (image, count, first few).
    pub uncovered: Vec<(String, usize, Vec<u64>)>,
    pub rootfs_files: usize,
}

/// Link `from` to `to` (a copy across file systems).
fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        return Ok(());
    }
    if std::fs::hard_link(from, to).is_err() {
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    Ok(())
}

/// Take a unit already built elsewhere (`GEX_EJECT_UNIT_CACHE`: units are
/// keyed by content, so one with the same key is the same unit).
fn reuse(key_dir: &Path, sources: &[PathBuf]) -> Result<bool> {
    let key = key_dir.file_name().unwrap();
    for s in sources {
        let src = s.join(key);
        if src.join("c.wasm").exists() && src.join("c.entries").exists() {
            std::fs::create_dir_all(key_dir)?;
            for f in ["c.entries", "c.cwasm", "c.wasm"] {
                if src.join(f).exists() {
                    link_or_copy(&src.join(f), &key_dir.join(f))?;
                }
            }
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn build(dir: &Path, project: &Project, config: &Config) -> Result<PeReport> {
    let _ = config;
    let ntss = nt_session(dir)?.ok_or_else(|| anyhow!("not a Windows program's project"))?;
    verify_rootfs(dir, project)?;
    let mut report = PeReport::default();

    // The guest's filesystem, then the exact layout in its prefix.
    let out = dir.join("build");
    let root = out.join("rootfs");
    if root.exists() {
        std::fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    }
    let _ = std::fs::remove_file(dir.join(BUILD_FILE));
    let mut n = 0;
    copy_tree(&dir.join("rootfs"), &root, &mut n)?;
    let user_files = dir.join("user/files");
    if user_files.is_dir() {
        copy_tree(&user_files, &root, &mut n)?;
    }
    report.rootfs_files = n;
    let prefix_host = root.join(rootfs_rel(&ntss.prefix)?.strip_prefix("rootfs")?);
    let wine = std::fs::read_to_string(prefix_host.join(".wine-version"))
        .with_context(|| format!("{}/.wine-version (the recorded prefix's)", ntss.prefix))?;
    let mut layout = format!("{}\nwine {}\nexact\n", nt::section::LAYOUT_VERSION, wine.trim());
    for m in images(&ntss) {
        let rel = m.path.strip_prefix(&format!("{}/", ntss.prefix)).unwrap_or(&m.path);
        layout.push_str(&format!("{:#x} {:#x} {} {}\n", m.base, m.size, m.id, rel));
    }
    std::fs::write(prefix_host.join(nt::section::LAYOUT_FILE), layout)?;

    // Units, into the project's own cache.
    let units = units_dir(dir);
    std::fs::create_dir_all(&units)?;
    std::env::set_var("GEX_CGEN_CACHE", &units);
    let sources: Vec<PathBuf> = std::env::var("GEX_EJECT_UNIT_CACHE")
        .map(|v| v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default();
    let jobs = std::env::var("JOBS").ok().and_then(|v| v.parse().ok()).unwrap_or(4usize);
    let class32 = abi_of(&ntss)? == nt::Abi::P32;
    if class32 {
        gex_xlate::set_compat(true);
    }
    let engine = gex_native::engine()?;
    let compile = |c_wasm: &Path| -> Result<()> {
        let bytes = std::fs::read(c_wasm)?;
        gex_cache::load_or_compile_at(&engine, &c_wasm.with_extension("cwasm"), &bytes)
            .with_context(|| format!("compiling {}", c_wasm.display()))?;
        Ok(())
    };
    // The NT trampoline page's.
    {
        let (image, _) = gex_kernel::nt::boot::trampoline_input(class32);
        let key = gex_cgen::image::image_key(&image);
        let made = reuse(&units.join(&key), &sources)?;
        let p = nt_cgen::prepare_trampolines(class32, jobs)?;
        compile(&p)?;
        report.units.push(("[nt trampolines]".into(), 0, 0, std::fs::metadata(&p)?.len(), if made { "reused" } else { "built" }));
    }
    let profile = std::fs::read_to_string(dir.join("session/profile.json"))
        .ok()
        .map(|t| gex_xlate::profile::Profile::from_json(&t).map_err(|e| anyhow!("session/profile.json: {e}")))
        .transpose()?;
    let mut images_out = Vec::new();
    for m in images(&ntss) {
        let host = root.join(rootfs_rel(&m.path)?.strip_prefix("rootfs")?);
        let bytes = std::fs::read(&host).with_context(|| format!("reading {}", host.display()))?;
        let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow!("{}: {e}", m.path))?;
        let key_dir = nt_cgen::unit_dir(&pe, m.base)?;
        let how = if key_dir.join("c.wasm").exists() {
            "present"
        } else if reuse(&key_dir, &sources)? {
            "reused"
        } else {
            eprintln!("  building the unit for {} at {:#x} ...", m.path, m.base);
            "built"
        };
        let p = nt_cgen::prepare_pe(&pe, m.base, jobs, true).with_context(|| format!("building the unit for {}", m.path))?;
        compile(&p)?;
        for name in nt_cgen::prepare_providers(&pe)? {
            if !report.providers.contains(&name) {
                report.providers.push(name);
            }
        }
        let entries = gex_cgen::image::read_entries(&p)?;
        // What the session ran on tier-1 in this image that the unit has no
        // entry for.
        if let Some(op) = profile.as_ref().and_then(|pr| pr.objects.get(&m.id)) {
            let bias = m.base.wrapping_sub(m.preferred);
            let set: std::collections::HashSet<u64> = entries.iter().copied().collect();
            let miss: Vec<u64> = op.blocks.keys().map(|o| o.wrapping_add(bias)).filter(|a| !set.contains(a)).collect();
            if !miss.is_empty() {
                report.uncovered.push((m.path.clone(), miss.len(), miss.iter().take(8).copied().collect()));
            }
        }
        report.units.push((m.path.clone(), m.base, entries.len(), std::fs::metadata(&p)?.len(), how));
        images_out.push(BuiltImage {
            kind: "unit".into(),
            identity: key_dir.file_name().unwrap().to_string_lossy().into_owned(),
            base: m.base,
            module: p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().into_owned(),
            object: m.id.clone(),
            path: m.path.clone(),
            blocks: entries.len() as u64,
            bytes: std::fs::metadata(&p)?.len(),
        });
    }
    let manifest = BuildManifest {
        inputs: crate::build::inputs_id(dir, &[])?,
        images: images_out,
        replacements: Vec::new(),
        pins: Vec::new(),
    };
    std::fs::write(dir.join(BUILD_FILE), manifest.to_json()).with_context(|| format!("writing {BUILD_FILE}"))?;
    Ok(report)
}

/// GEX_NT_* settings a run replays from the session unless set now: those
/// that change what the program does (not diagnostics).
fn replayed_setting(name: &str) -> bool {
    !matches!(
        name,
        "GEX_NT_STATS" | "GEX_NT_PEEK" | "GEX_NT_TRACE" | "GEX_NT_TRACE_IMAGES" | "GEX_NT_HANG_DUMP" | "GEX_NT_HANG_SECS" | "GEX_NT_CGEN_TRACE"
            | "GEX_NT_CGEN_BUILD" | "GEX_NT_CGEN_SYNC"
    )
}

pub fn run(dir: &Path, flags: &crate::run::RunFlags) -> Result<Exit> {
    let ntss = nt_session(dir)?.ok_or_else(|| anyhow!("not a Windows program's project"))?;
    let project = Project::load(dir)?;
    let manifest = BuildManifest::load(dir)?;
    if crate::build::inputs_id(dir, &[])? != manifest.inputs {
        bail!("{} changed since it was built (gex-project.json or user/eject.json); run ./build.sh", dir.display());
    }
    let (config, _) = Config::load(dir)?;
    let root = std::fs::canonicalize(crate::build::rootfs(dir)).with_context(|| format!("{} (run ./build.sh)", dir.display()))?;
    let units = std::fs::canonicalize(units_dir(dir))?;
    for s in &ntss.settings {
        if let Some((k, v)) = s.split_once('=') {
            if replayed_setting(k) && std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
    }
    std::env::set_var("GEX_CGEN_CACHE", &units);
    std::env::set_var("GEX_NT_CGEN_BUILD", "0");
    let jit = flags.jit_fallback || config.jit_fallback;
    nt_cgen::set_mode(if jit { nt_cgen::Mode::Default } else { nt_cgen::Mode::Aot });
    let args: Vec<String> = match (&flags.args, &config.args) {
        (Some(a), _) => a.clone(),
        (None, Some(a)) => a.clone(),
        (None, None) => ntss.args.clone(),
    };
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0");
    let env: Vec<(String, String)> =
        ntss.env.iter().filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let cfg = nt::Config {
        prefix: ntss.prefix.clone(),
        image_path: ntss.image_path.clone(),
        cmdline: String::new(),
        env,
        cwd: ntss.cwd.clone(),
        trace: flags.trace || flag("GEX_NT_TRACE"),
        strict: flag("GEX_NT_STRICT"),
        settings: Vec::new(),
    };
    let setup = gex_native::nt::NtSetup { abi: abi_of(&ntss)?, exe_path: ntss.exe.clone(), args: args.clone(), cfg, display_out: flags.display.clone() };
    let mut argv = vec![ntss.exe.clone()];
    argv.extend(args);
    let opts = RunOptions {
        args: argv,
        trace: flags.trace,
        verbose: flags.verbose,
        sysroot: Some(root),
        cwd: project.program.cwd.clone(),
        gl: ntss.gl,
        wayland_out: flags.display.clone(),
        aot_only: !jit,
        ..RunOptions::default()
    };
    let (exit, _) = gex_native::nt::run_nt_program(&opts, &gex_xlate::Options::default(), &setup)?;
    Ok(exit)
}
