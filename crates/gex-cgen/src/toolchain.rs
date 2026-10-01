//! Driving clang and wasm-ld.
//!
//! Translation units compile in parallel, one clang process each; the objects
//! link into one standalone module that the splice then takes apart. Missing
//! tools are an error, never a quiet fallback to tier-1.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// clang's flags for generated C. See `docs/function-tier.md` §3 and §8.
pub const CFLAGS: &[&str] = &[
    "--target=wasm32",
    "-O2",
    // No switch-to-lookup-table conversion: its tables are data segments,
    // which the splice cannot place (a guest-memory address); jump tables
    // (br_table) are unaffected.
    "-mllvm",
    "-switch-to-lookup=false",
    "-mtail-call",
    "-msimd128",
    "-matomics",
    "-mbulk-memory",
    "-mnontrapping-fptoint",
    "-msign-ext",
    "-fno-strict-aliasing",
    "-fno-delete-null-pointer-checks",
    "-ffp-contract=off",
    "-fno-math-errno",
    "-ffreestanding",
    "-nostdlib",
    "-fno-exceptions",
    "-Wno-unused-label",
    "-Wno-unused-variable",
    "-Wno-unused-but-set-variable",
];

fn tool(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

pub fn clang() -> String {
    tool("GEX_CLANG", "clang")
}

pub fn wasm_ld() -> String {
    tool("GEX_WASM_LD", "wasm-ld")
}

/// A scratch directory for one build: `GEX_CGEN_KEEP` if set (kept), else a
/// fresh temporary directory (removed on drop).
pub struct WorkDir {
    pub path: PathBuf,
    keep: bool,
}

impl WorkDir {
    pub fn new() -> Result<WorkDir> {
        if let Some(d) = std::env::var_os("GEX_CGEN_KEEP") {
            let path = PathBuf::from(d);
            std::fs::create_dir_all(&path)
                .with_context(|| format!("creating {}", path.display()))?;
            return Ok(WorkDir { path, keep: true });
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("gex-cgen-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(WorkDir { path, keep: false })
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd
        .output()
        .with_context(|| format!("running {what} ({:?}); is it installed?", cmd.get_program()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = err.lines().take(60).collect();
        bail!("{what} failed ({}):\n{}", out.status, lines.join("\n"));
    }
    Ok(())
}

/// How many clang processes at once: `GEX_CGEN_JOBS`, else the CPUs this
/// process may run on.
pub fn jobs() -> usize {
    std::env::var("GEX_CGEN_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
        .max(1)
}

/// `clang --version`, for object-cache keys.
pub fn clang_version() -> Result<String> {
    let out = Command::new(clang())
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version; is it installed?", clang()))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The flags every unit is compiled with, as one string, for cache keys.
pub fn cflags_key() -> String {
    let mut v: Vec<String> = CFLAGS.iter().map(|s| s.to_string()).collect();
    v.extend(extra_cflags());
    v.join(" ")
}

/// Precompile `dir/gex_abi.h` with the units' flags; returns the `.pch`.
pub fn precompile_header(dir: &Path) -> Result<PathBuf> {
    let pch = dir.join("gex_abi.h.pch");
    let mut cmd = Command::new(clang());
    cmd.args(CFLAGS)
        .args(extra_cflags())
        .arg("-x")
        .arg("c-header")
        .arg(dir.join("gex_abi.h"))
        .arg("-o")
        .arg(&pch);
    run(&mut cmd, "clang (precompiling gex_abi.h)")?;
    Ok(pch)
}

/// Compile one unit to `obj` (written beside it first, then renamed, so a
/// killed build leaves no truncated object in a cache).
pub fn compile_one(dir: &Path, src: &Path, obj: &Path, pch: Option<&Path>) -> Result<()> {
    let tmp = obj.with_extension(format!("o.tmp-{}", std::process::id()));
    let mut cmd = Command::new(clang());
    cmd.args(CFLAGS).args(extra_cflags()).arg("-I").arg(dir);
    if let Some(p) = pch {
        cmd.arg("-include-pch").arg(p);
    }
    cmd.arg("-c").arg(src).arg("-o").arg(&tmp);
    run(&mut cmd, &format!("clang on {}", src.display()))?;
    std::fs::rename(&tmp, obj)?;
    Ok(())
}

/// Compile every `.c` in `sources` to an object beside it, in parallel.
pub fn compile_all(dir: &Path, sources: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let objs: Vec<PathBuf> = sources.iter().map(|s| s.with_extension("o")).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let errors = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs().min(sources.len().max(1)) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= sources.len() {
                    return;
                }
                let mut cmd = Command::new(clang());
                cmd.args(CFLAGS)
                    .args(extra_cflags())
                    .arg("-I")
                    .arg(dir)
                    .arg("-c")
                    .arg(&sources[i])
                    .arg("-o")
                    .arg(&objs[i]);
                if let Err(e) = run(&mut cmd, &format!("clang on {}", sources[i].display())) {
                    errors.lock().unwrap().push(e);
                }
            });
        }
    });
    if let Some(e) = errors.into_inner().unwrap().into_iter().next() {
        return Err(e);
    }
    Ok(objs)
}

/// Link objects into one module exporting `exports`.
pub fn link(dir: &Path, objs: &[PathBuf], exports: &[String], out: &Path) -> Result<()> {
    // A response file: a closure can have a hundred thousand exports.
    let rsp = dir.join("link.rsp");
    let mut text = String::new();
    for e in exports {
        text.push_str("--export=");
        text.push_str(e);
        text.push('\n');
    }
    for o in objs {
        text.push_str(&o.to_string_lossy());
        text.push('\n');
    }
    std::fs::write(&rsp, text)?;
    let mut cmd = Command::new(wasm_ld());
    cmd.args([
        "--no-entry",
        "--import-memory",
        "--shared-memory",
        "--max-memory=4294901760",
        "--allow-undefined",
        "-O2",
    ])
    .arg(format!("@{}", rsp.display()))
    .arg("-o")
    .arg(out);
    run(&mut cmd, "wasm-ld")
}

/// `GEX_CGEN_CFLAGS`: extra clang flags, for pricing code-generation choices
/// (not part of any cache key: build with `--no-cache` when using it).
fn extra_cflags() -> Vec<String> {
    std::env::var("GEX_CGEN_CFLAGS")
        .map(|v| v.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}
