//! `gexc run` on a Windows program (docs/nt-layer.md §8, §10): Wine 11.15's
//! PE DLLs from a staged prefix (`tools/nt-stage-wine.sh`) on gex's NT layer.

use crate::RunArgs;
use anyhow::{bail, Context, Result};
use gex_kernel::nt;
use gex_native::nt::NtSetup;
use gex_native::Exit;

/// The staged prefix: `--wine-prefix`, `$GEX_WINE_PREFIX`, or
/// `~/.cache/gex/wine-11.15`.
fn prefix(ra: &RunArgs) -> Result<String> {
    let p = match (&ra.wine_prefix, std::env::var("GEX_WINE_PREFIX")) {
        (Some(p), _) => p.clone(),
        (None, Ok(p)) => p,
        (None, Err(_)) => format!("{}/.cache/gex/wine-11.15", std::env::var("HOME").unwrap_or_default()),
    };
    let p = std::fs::canonicalize(&p).with_context(|| {
        format!("no Wine prefix at {p}; stage one with tools/nt-stage-wine.sh (or pass --wine-prefix)")
    })?;
    let version = std::fs::read_to_string(p.join(".wine-version")).unwrap_or_default();
    if version.trim() != "11.15" {
        bail!("{} is not a Wine 11.15 prefix staged by tools/nt-stage-wine.sh", p.display());
    }
    Ok(p.to_string_lossy().into_owned())
}

/// The Windows environment: the variables Wine's prefix would give a
/// program, then `--env` overrides. The Linux environment is not passed on
/// (it means nothing to a Windows program), except `WINEDEBUG`, which Wine's
/// PE DLLs read to decide what to trace, and `WINEDLLOVERRIDES`, the load
/// order the NT layer applies to image sections.
fn environment(extra: &[String]) -> Vec<(String, String)> {
    let mut env = nt::boot::default_environment();
    let mut set = |k: &str, v: &str| match env.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(k)) {
        Some(e) => e.1 = v.to_string(),
        None => env.push((k.to_string(), v.to_string())),
    };
    // And the load order (DllOverrides), which Wine reads the same way.
    for k in ["WINEDEBUG", "WINEDLLOVERRIDES"] {
        if let Ok(v) = std::env::var(k) {
            set(k, &v);
        }
    }
    for e in extra {
        if let Some((k, v)) = e.split_once('=') {
            set(k, v);
        }
    }
    env
}

pub fn run(ra: &RunArgs, bytes: &[u8]) -> Result<Exit> {
    if !gex_abi::X86 {
        bail!("{} is a Windows program; running one needs gexc built with --features x86", ra.path);
    }
    if ra.opts.sysroot.is_some() {
        bail!("--sysroot is not supported for Windows programs (the NT layer maps C: to the Wine prefix)");
    }
    let abi = nt::boot::exe_abi(bytes).with_context(|| format!("{} is not an x86 or x86-64 PE program", ra.path))?;
    let prefix = prefix(ra)?;
    let exe_path = std::fs::canonicalize(&ra.path)?.to_string_lossy().into_owned();
    let cwd = match &ra.opts.cwd {
        Some(d) => d.clone(),
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0");
    let cfg = nt::Config {
        prefix: prefix.clone(),
        image_path: nt::boot::dos_path(&prefix, &exe_path),
        cmdline: String::new(),
        env: environment(&ra.opts.env),
        cwd: nt::boot::dos_path(&prefix, &cwd),
        trace: ra.opts.trace || flag("GEX_NT_TRACE"),
        strict: flag("GEX_NT_STRICT"),
        // Read from the process environment (Process::setting).
        settings: Vec::new(),
    };
    let setup = NtSetup { abi, exe_path, args: ra.guest_args.clone(), cfg, display_out: None };
    let (exit, _stats) = gex_native::nt::run_nt_program(&ra.opts, &ra.xlate, &setup)?;
    Ok(exit)
}
