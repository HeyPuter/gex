//! `gexc eject`: the command line of `gex-eject` (see that crate and
//! docs/eject.md).

use anyhow::{bail, Context, Result};
use gex_native::Exit;
use std::path::PathBuf;

pub const USAGE: &str = "  gexc eject <record-dir> --out <project> [--build]
                                   write a standalone project from a session
                                   `gexc run --record` recorded: the closure,
                                   the files it read, the runtime, a build step
                                   and a launcher (docs/eject.md)
  gexc eject build <project>       translate it ahead of time and bind the
                                   user's replacements (the project's build.sh)
  gexc eject run <project> [--jit-fallback] [--strace] [--verbose] [--] [args...]
                                   run it natively (the project's run.sh); code
                                   the build never saw fails loudly unless
                                   --jit-fallback";

/// What the caller reports: an exit to pass on, or nothing.
pub fn cmd_eject(args: &[String]) -> Result<Option<Exit>> {
    match args.first().map(String::as_str) {
        Some("build") => {
            let dir = PathBuf::from(args.get(1).context("gexc eject build <project>")?);
            if args.len() > 2 {
                bail!("gexc eject build takes only the project directory");
            }
            build(&dir)?;
            Ok(None)
        }
        Some("run") => {
            let dir = PathBuf::from(args.get(1).context("gexc eject run <project> [args...]")?);
            let mut flags = gex_eject::run::RunFlags::default();
            let mut rest = Vec::new();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--jit-fallback" => flags.jit_fallback = true,
                    "--strace" => flags.trace = true,
                    "--verbose" | "-v" => flags.verbose = true,
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
            Ok(Some(gex_eject::run::run(&dir, &flags)?))
        }
        Some("decompile") => bail!("gexc eject decompile: not yet (eject v1, with the function tier)"),
        Some(_) => {
            let mut record = None;
            let mut out = None;
            let mut then_build = false;
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "--out" | "-o" => {
                        i += 1;
                        out = Some(PathBuf::from(args.get(i).context("--out needs a directory")?));
                    }
                    "--build" => then_build = true,
                    o if o.starts_with('-') => bail!("gexc eject: unknown option {o}"),
                    r => {
                        if record.replace(PathBuf::from(r)).is_some() {
                            bail!("gexc eject: one record directory at a time");
                        }
                    }
                }
                i += 1;
            }
            let record = record.context("gexc eject <record-dir> --out <project>")?;
            let out = out.context("gexc eject: where to? (--out <project>)")?;
            let runtime = std::env::current_exe().context("finding this gexc to copy into the project")?;
            let s = gex_eject::eject::eject(
                &record,
                &out,
                &gex_eject::eject::EjectOptions {
                    runtime,
                    runtime_build: gex_cache::build_fingerprint(),
                },
            )?;
            eprintln!(
                "{}: {} objects, {} files ({} KiB), {} missing paths kept missing, {} outputs left out{}",
                out.display(),
                s.objects,
                s.files,
                s.bytes / 1024,
                s.missing,
                s.outputs,
                if s.kept_user { "; user/ kept" } else { "" }
            );
            eprintln!(
                "  the project holds the program's own code and the files it read; \
                 distributing it is subject to their licences"
            );
            if then_build {
                build(&out)?;
            }
            Ok(None)
        }
        None => bail!("usage:\n{USAGE}"),
    }
}

fn build(dir: &std::path::Path) -> Result<()> {
    let r = gex_eject::build::build(dir)?;
    for i in &r.images {
        eprintln!("  {:<40} {:>7} blocks {:>7} KiB  {}", i.path, i.blocks, i.bytes / 1024, i.module);
    }
    for x in &r.replacements {
        eprintln!(
            "  replace {} at {:#x} with {}:{} ({}, {})",
            x.symbol,
            x.addr,
            x.module,
            x.export,
            x.abi.name(),
            x.sig
        );
    }
    if r.illegal_blocks > 0 {
        eprintln!(
            "  {} blocks end in an instruction gex cannot execute yet; reaching one raises SIGILL",
            r.illegal_blocks
        );
    }
    eprintln!("{}: built ({} files in build/rootfs)", dir.display(), r.rootfs_files);
    Ok(())
}
