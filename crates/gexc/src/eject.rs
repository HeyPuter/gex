//! `gexc eject`: the command line of `gex-eject` (see that crate and
//! docs/eject.md).

use anyhow::{bail, Context, Result};
use gex_native::Exit;
use std::path::PathBuf;

pub const USAGE: &str = "  gexc eject <record-dir> --out <project> [--build] [--web-shim gex_browser.wasm]
                                   write a standalone project from a session
                                   `gexc run --record` recorded: the closure,
                                   the files it read, the runtime, a build step
                                   and a launcher (docs/eject.md)
  gexc eject <wine32-record-dir> --out <project> --web-src <gex>/web [--unit-cache DIR]... [--features -compositor,...] [--build]
                                   the same for a wine32 page (Wine as wasm32)
                                   recorded by tools/eject/record-wine32.mjs: its
                                   bundle, units and runtime, and the features
                                   the build keeps (docs/eject.md, \"wine32 pages\")
  gexc eject build <project>       translate it ahead of time and bind the
                                   user's replacements (the project's build.sh)
  gexc eject override <project> <object> <symbol|0xoffset> [--size N]
                                   print a user/eject.json replacement entry
                                   for a function, with the object's hash and
                                   the function's bytes' hash, so a changed
                                   binary is reported instead of mis-bound
  gexc eject decompile <project> <object> <symbol|0xoffset> [--size N] [--callees] [--force]
                                   write the function tier's C for a function
                                   into user/src/ as an editable override
                                   (\"entry\" ABI), and print its eject.json entry
  gexc eject run <project> [--jit-fallback] [--strace] [--verbose] [--wayland-out P|null] [--] [args...]
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
        Some("prune") => {
            // A development view of the cross-image fan-out analysis: the
            // report, without building the site.
            let dir = PathBuf::from(args.get(1).context("gexc eject prune <project>")?);
            let (out, keeps) = gex_eject::fanout::project(&dir)?;
            print!("{}", gex_eject::fanout::report(&out, &keeps));
            Ok(None)
        }
        Some("prune-check") => {
            // The fan-out proof against a real run: a console log of a site
            // built with "cold": "instrument" names every function that ran.
            let dir = PathBuf::from(args.get(1).context("gexc eject prune-check <project> <console log>")?);
            let log = args.get(2).context("gexc eject prune-check <project> <console log>")?;
            let text = std::fs::read_to_string(log)?;
            print!("{}", gex_eject::fanout::check(&dir, &text)?);
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
                    "--wayland-out" => {
                        i += 1;
                        flags.display = Some(args.get(i).context("--wayland-out needs a file prefix or `null`")?.clone());
                    }
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
        Some("override") => {
            // gexc eject override <project> <object> <symbol|0xoffset> [--size N]
            let mut pos = Vec::new();
            let mut size = None;
            let mut i = 1;
            while i < args.len() {
                if args[i] == "--size" {
                    i += 1;
                    let v = args.get(i).context("--size needs a number")?;
                    size = Some(match v.strip_prefix("0x") {
                        Some(h) => u64::from_str_radix(h, 16)?,
                        None => v.parse()?,
                    });
                } else {
                    pos.push(args[i].clone());
                }
                i += 1;
            }
            let [dir, object, what] = pos.as_slice() else {
                bail!("gexc eject override <project> <object> <symbol|0xoffset> [--size N]");
            };
            print!("{}", gex_eject::build::describe(std::path::Path::new(dir), object, what, size)?);
            Ok(None)
        }
        Some("decompile") => {
            // gexc eject decompile <project> <object> <symbol|0xoffset> [--size N] [--callees] [--force]
            let mut pos = Vec::new();
            let (mut size, mut callees, mut force) = (None, false, false);
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--size" => {
                        i += 1;
                        let v = args.get(i).context("--size needs a number")?;
                        size = Some(match v.strip_prefix("0x") {
                            Some(h) => u64::from_str_radix(h, 16)?,
                            None => v.parse()?,
                        });
                    }
                    "--callees" => callees = true,
                    "--force" => force = true,
                    o if o.starts_with("--") => bail!("gexc eject decompile: unknown option {o}"),
                    _ => pos.push(args[i].clone()),
                }
                i += 1;
            }
            let [dir, object, what] = pos.as_slice() else {
                bail!("gexc eject decompile <project> <object> <symbol|0xoffset> [--size N] [--callees] [--force]");
            };
            let d = gex_eject::decompile::decompile(std::path::Path::new(dir), object, what, size, callees, force)?;
            eprintln!(
                "{}: {} function{} of C; bind it in user/eject.json with:",
                d.file,
                d.functions,
                if d.functions == 1 { "" } else { "s" }
            );
            print!("{}", d.entry);
            Ok(None)
        }
        Some(_) => {
            let mut record = None;
            let mut out = None;
            let mut then_build = false;
            let mut web_shim = None;
            let mut web_src = None;
            let mut unit_cache: Vec<PathBuf> = std::env::var("GEX_EJECT_UNIT_CACHE")
                .map(|v| v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect())
                .unwrap_or_default();
            let mut features_off = Vec::new();
            let mut i = 0;
            while i < args.len() {
                match args[i].as_str() {
                    "--out" | "-o" => {
                        i += 1;
                        out = Some(PathBuf::from(args.get(i).context("--out needs a directory")?));
                    }
                    "--build" => then_build = true,
                    "--web-shim" => {
                        i += 1;
                        web_shim = Some(PathBuf::from(args.get(i).context("--web-shim needs gex_browser.wasm")?));
                    }
                    "--web-src" => {
                        i += 1;
                        web_src = Some(PathBuf::from(args.get(i).context("--web-src needs gex's web/ directory")?));
                    }
                    "--unit-cache" => {
                        i += 1;
                        unit_cache.push(PathBuf::from(args.get(i).context("--unit-cache needs a directory")?));
                    }
                    "--features" => {
                        i += 1;
                        for f in args.get(i).context("--features needs a list, e.g. -compositor")?.split(',') {
                            match f.strip_prefix('-') {
                                Some(n) => features_off.push(n.to_string()),
                                None => bail!("--features: \"{f}\": say which to leave out, as -NAME (everything else is in)"),
                            }
                        }
                    }
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
            if gex_eject::wine32::is_recording(&record) {
                let web_src = web_src.context("a wine32 recording needs --web-src <gex>/web (the page's source and the built runtime)")?;
                let s = gex_eject::wine32::eject(
                    &record,
                    &out,
                    &runtime,
                    &gex_eject::wine32::Wine32Options { web_src, unit_cache, features_off },
                )?;
                eprintln!(
                    "{}: a wine32 page: {} processes, {} units and {} provider modules ({} MiB), bundle {} MiB, {} wasm modules{}",
                    out.display(),
                    s.processes,
                    s.units,
                    s.providers,
                    s.unit_bytes >> 20,
                    s.bundle_bytes >> 20,
                    s.modules,
                    if s.kept_user { "; user/ kept" } else { "" }
                );
                eprintln!(
                    "  the project holds the program's own code and the files it read; \
                     distributing it is subject to their licences"
                );
                if then_build {
                    build(&out)?;
                }
                return Ok(None);
            }
            if web_src.is_some() || !features_off.is_empty() {
                bail!("--web-src and --features are for a wine32 recording (wine32-session.json)");
            }
            let s = gex_eject::eject::eject(
                &record,
                &out,
                &gex_eject::eject::EjectOptions {
                    runtime,
                    runtime_build: gex_cache::build_fingerprint(),
                    web_shim,
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
            if !s.web {
                eprintln!("  no web build: give --web-shim <gex_browser.wasm> for run.mjs and index.html");
            }
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
    if let Some(w) = &r.w3 {
        eprintln!(
            "{}: wine32 page built: {} page files, {} units; features off: {}",
            dir.display(),
            w.page_files,
            w.units,
            if w.features_off.is_empty() { "none".to_string() } else { w.features_off.join(", ") }
        );
        return Ok(());
    }
    for i in &r.images {
        eprintln!("  {:<40} {:>7} blocks {:>7} KiB  {}", i.path, i.blocks, i.bytes / 1024, i.module);
    }
    let pl = &r.placement;
    let mib = |b: u64| b as f64 / (1u64 << 20) as f64;
    eprintln!(
        "  placed {} of {} libraries ({:.1} of {:.1} MiB){}",
        pl.placed,
        pl.libraries,
        mib(pl.placed_bytes),
        mib(pl.library_bytes),
        if pl.placed < pl.libraries { "; the rest translate in each process" } else { "" }
    );
    let missing = r.replacements.iter().filter(|x| x.missing).count();
    for x in r.replacements.iter().filter(|x| !x.missing) {
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
    if missing > 0 {
        eprintln!(
            "  {missing} functions of replaced libraries have no replacement and fail loudly if \
             called: {}",
            r.replacements.iter().filter(|x| x.missing).map(|x| x.symbol.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    if r.illegal_blocks > 0 {
        eprintln!(
            "  {} blocks end in an instruction gex cannot execute yet; reaching one raises SIGILL",
            r.illegal_blocks
        );
    }
    match &r.web {
        Some(Ok(bytes)) => eprintln!("  web build: {} KiB in build/web (node run.mjs, or serve index.html)", bytes / 1024),
        Some(Err(why)) => eprintln!("  no web build: {why}"),
        None => {}
    }
    eprintln!("{}: built ({} files in build/rootfs)", dir.display(), r.rootfs_files);
    Ok(())
}
