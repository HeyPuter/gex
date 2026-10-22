//! `gex-rt`: the runtime of an ejected gex project.
//!
//! It runs what the project's build made -- function-tier units, export
//! providers, and every runtime module, all compiled ahead of time -- and
//! nothing else: it is built without gex's translators (tier-1, the function
//! JIT, unit builds) and without a wasm compiler, so code nobody built
//! cannot run, by construction rather than by a flag.
//!
//! ```text
//! gex-rt <project> [--strace] [--verbose] [--wayland-out PREFIX|null] [--] [args...]
//! ```

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The native unit flavor (no bounds guard), as gexc's native commands:
    // part of every unit's key.
    gex_cgen::image::set_unguarded(std::env::var("GEX_CGEN_GUARD").as_deref() != Ok("1"));
    // The first guest thread needs a guest-sized stack, as gexc's.
    let r = std::thread::Builder::new()
        .name("gex-main".into())
        .stack_size(gex_native::guest_thread_stack())
        .spawn(move || run(&args))
        .expect("spawning the main guest thread")
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p));
    match r {
        Ok(gex_native::Exit::Status(c)) => ExitCode::from(c as u8),
        Ok(gex_native::Exit::Signal { signal, detail, .. }) => {
            eprintln!("gex-rt: {detail}");
            ExitCode::from((128 + signal) as u8)
        }
        Err(e) => {
            eprintln!("gex-rt: {e:#}");
            ExitCode::from(70)
        }
    }
}

fn run(args: &[String]) -> Result<gex_native::Exit> {
    let dir = PathBuf::from(args.first().context("usage: gex-rt <project> [--strace] [--verbose] [--wayland-out P|null] [--] [args...]")?);
    let mut flags = gex_eject::run::RunFlags::default();
    let mut rest = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--strace" => flags.trace = true,
            "--verbose" | "-v" => flags.verbose = true,
            "--wayland-out" => {
                i += 1;
                flags.display = Some(args.get(i).context("--wayland-out needs a file prefix or `null`")?.clone());
            }
            "--jit-fallback" => bail!("gex-rt has no translator; run the project with gexc for --jit-fallback"),
            "--" => {
                rest.extend(args[i + 1..].iter().cloned());
                flags.args = Some(Vec::new());
                break;
            }
            _ => {
                rest.extend(args[i..].iter().cloned());
                break;
            }
        }
        i += 1;
    }
    if !rest.is_empty() {
        flags.args = Some(rest);
    }
    gex_eject::pe::run(&dir, &flags)
}
