//! `gexc` — the gex command-line driver.
//!
//! M0 subcommands (PLAN §11):
//!
//! ```text
//!   gexc run <elf> [args...]   translate and execute, natively
//!   gexc build <elf> -o <out>  translate and write the wasm module
//!   gexc disasm <elf>          disassemble, to check the decoder by eye
//!   gexc ir <elf> --at <addr>  dump the GIR for one basic block
//! ```

use anyhow::{bail, Context, Result};
use gex_native::{Exit, RunOptions};
use gex_xlate::Options;
use std::process::ExitCode;

/// Where the dynamic linker is placed, well above a PIE executable so their
/// mappings cannot overlap.
const INTERP_BASE: u64 = 0x2800_0000;

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("gexc: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn real_main() -> Result<ExitCode> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        usage();
        return Ok(ExitCode::from(2));
    }
    match argv[1].as_str() {
        "run" => cmd_run(&argv[2..]),
        "build" => cmd_build(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "seed" => cmd_seed(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "disasm" => cmd_disasm(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "ir" => cmd_ir(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "-h" | "--help" | "help" => {
            usage();
            Ok(ExitCode::SUCCESS)
        }
        other => bail!("unknown subcommand `{other}` (try `gexc help`)"),
    }
}

fn usage() {
    eprintln!(
        "gex — AArch64 to WebAssembly ahead-of-time compiler and Linux runtime

usage:
  gexc run    <elf> [args...]      translate and run the program
  gexc build  <elf> -o <out.wasm>  translate and write the wasm module
                                   (--exhaustive: translate every decodable
                                   address, since a built module has no
                                   runtime translator behind it;
                                   --no-shared-memory: declare a plain memory,
                                   for a single-threaded host;
                                   --no-validate: write the module even if it
                                   is not valid wasm, for inspecting one that
                                   is not)
  gexc seed   -o <out.wasm>        write a module that translates nothing, for
                                   a host that will translate at runtime and
                                   does not know the program in advance
  gexc disasm <elf>                disassemble the executable segments
  gexc ir     <elf> --at <addr>    print the GIR for one basic block

run options:
  --strace        trace every syscall on stderr
  --verbose       report translation statistics
  --env K=V       add an environment variable (repeatable)
  --sysroot DIR   present DIR as the guest's root filesystem
  --uid N[:M]     the uid (and gid) the guest sees; `--uid 0` is root, which
                  is what a package manager inside a sysroot expects
  --sample        report the guest's hottest back-edges when it exits. Biased
                  towards loops, which is the bias you want when the question
                  is where the time went
  --wayland PATH  run a Wayland compositor and bind it at PATH inside the
                  guest, so a guest client can open a window. WAYLAND_DISPLAY
                  and XDG_RUNTIME_DIR are set to match unless already given
  --wayland-size WxH
                  what size to tell a toplevel window it is (default 640x480)
  --wayland-out PATH
                  write each composited screen as PATH-NNN.ppm
  --gl            give the guest /dev/gex-gl, so a program linked against the
                  guest's libGLESv2 runs its OpenGL on the host's
  --virgl [PATH]  where to serve the virtio-gpu renderer, so the guest's own
                  Mesa draws on the host's GPU. Unmodified OpenGL programs
                  work through this; set GALLIUM_DRIVER=virpipe in the guest.
                  On by default at /tmp/.virgl_test, which is where Mesa
                  looks, in a build that has libvirglrenderer.
  --no-virgl      do not serve one
  --wayland-gpu   composite surfaces on the GPU through WebGPU, rather than
                  handing the host one client buffer at a time. Falls back to
                  the CPU, with a note, when there is no adapter
  --recompile-objects
                  statically recompile each shared library as it is mapped,
                  rather than translating it a dispatch miss at a time. Off by
                  default: measured slower, because seeding from every
                  decodable address translates far more of a library than any
                  one program uses. Measured on Debian bash and glibc, with
                  compilation parallel and the element-segment limit fixed:
                  cold 14s against the runtime translator's 8s, and 4s
                  against 1s once both are cached. What it buys is the
                  absence of translation pauses part way through a run, not
                  speed -- an X server that dlopens a 24 MB driver mid-frame
                  is the case that wants it.
  --profile       reuse the previous run's discovered entry points. Off by
                  default: it changes the translation and so the cache key,
                  which costs a whole recompilation on the second run of a
                  program -- measured at five to six seconds on bash and
                  busybox, for no measurable gain once both settle
  --exhaustive    translate every decodable text address ahead of time
  --no-jit        no runtime translation; implies --exhaustive
  --lazy          discover only from the entry point, leaving the rest to
                  runtime translation; much faster to start on a big binary
  --no-cache      do not reuse compiled modules between runs
  --cwd DIR       start in this guest directory
  --argv0 NAME    what the guest sees as argv[0]; useful with --sysroot,
                  where the host path the program was loaded from is not a
                  path the guest can resolve"
    );
}

/// Split gexc's own flags from the guest's argument vector.
struct RunArgs {
    path: String,
    /// What the guest should see as `argv[0]`, if not `path`.
    argv0: Option<String>,
    guest_args: Vec<String>,
    opts: RunOptions,
    xlate: Options,
    jit: bool,
}

fn parse_run_args(args: &[String]) -> Result<RunArgs> {
    let mut path = None;
    let mut guest_args = Vec::new();
    let mut opts = RunOptions::default();
    // Ahead-of-time discovery is best-effort; runtime translation fills the
    // gaps. Translating every decodable address up front is the fallback for
    // when it is turned off.
    let mut exhaustive = false;
    let mut no_jit = false;
    let mut lazy = false;
    let mut argv0: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        // Once the program is named, everything else belongs to the guest.
        if path.is_some() {
            guest_args.push(a.clone());
            i += 1;
            continue;
        }
        match a.as_str() {
            "--strace" => opts.trace = true,
            "--verbose" | "-v" => opts.verbose = true,
            "--exhaustive" => exhaustive = true,
            "--sample" => opts.sample = true,
            "--recompile-objects" => opts.recompile_objects = true,
            "--profile" => opts.profile = true,
            // A display server for the guest, bound at a path in its own
            // unix-socket namespace. Clients find it through WAYLAND_DISPLAY
            // and XDG_RUNTIME_DIR, which are set below if the caller did not.
            "--wayland" => {
                i += 1;
                opts.wayland = Some(args.get(i).context("--wayland needs a path")?.clone());
            }
            "--wayland-size" => {
                i += 1;
                let spec = args.get(i).context("--wayland-size needs WxH")?;
                let (w, h) = spec.split_once('x').context("--wayland-size wants WxH")?;
                opts.wayland_size = (w.parse()?, h.parse()?);
            }
            "--wayland-gpu" => opts.wayland_gpu = true,
            "--virgl" => {
                // The path is optional: Mesa has a default and almost nobody
                // overrides it, so requiring it would be noise.
                let next = args.get(i + 1);
                let path = match next {
                    Some(p) if !p.starts_with('-') => {
                        i += 1;
                        p.clone()
                    }
                    _ => gex_native::virgl::DEFAULT_SOCKET.to_string(),
                };
                opts.virgl = Some(path);
            }
            "--no-virgl" => opts.virgl = None,
            "--gl" => opts.gl = true,
            "--wayland-out" => {
                i += 1;
                opts.wayland_out =
                    Some(args.get(i).context("--wayland-out needs a path")?.clone());
            }
            "--uid" => {
                i += 1;
                let v = args.get(i).context("--uid needs a number")?;
                let (u, g) = match v.split_once(':') {
                    Some((u, g)) => (u, g),
                    None => (v.as_str(), v.as_str()),
                };
                opts.identity = Some((u.parse().context("--uid")?, g.parse().context("--uid")?));
            }
            "--sysroot" => {
                i += 1;
                let v = args.get(i).context("--sysroot needs a directory")?;
                opts.sysroot = Some(v.into());
            }
            "--no-jit" => no_jit = true,
            "--lazy" => lazy = true,
            "--no-cache" => opts.cache = false,
            "--cwd" => {
                i += 1;
                let v = args.get(i).context("--cwd needs a directory")?;
                opts.cwd = Some(v.clone());
            }
            "--argv0" => {
                i += 1;
                let v = args.get(i).context("--argv0 needs a name")?;
                argv0 = Some(v.clone());
            }
            "--env" => {
                i += 1;
                let v = args.get(i).context("--env needs an argument")?;
                opts.env.push(v.clone());
            }
            other if other.starts_with('-') => bail!("unknown option `{other}`"),
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected a program to run")?;
    Ok(RunArgs {
        path,
        argv0,
        guest_args,
        opts,
        xlate: Options {
            exhaustive: exhaustive || no_jit,
            lazy: lazy && !no_jit,
            ..Options::default()
        },
        jit: !no_jit,
    })
}

fn cmd_run(args: &[String]) -> Result<ExitCode> {
    let mut ra = parse_run_args(args)?;
    let bytes = std::fs::read(&ra.path).with_context(|| format!("reading {}", ra.path))?;
    // The interpreter is resolved through the guest's namespace — the sysroot
    // when one is given — so a program links against the libraries its own
    // root provides rather than the host's.
    let sysroot = ra.opts.sysroot.clone();
    let program = gex_ld::load_program(&bytes, gex_abi::PIE_BASE, INTERP_BASE, &|guest_path| {
        let host = match &sysroot {
            Some(root) => root.join(guest_path.trim_start_matches('/')),
            None => std::path::PathBuf::from(guest_path),
        };
        std::fs::read(host).ok()
    })
    .with_context(|| format!("loading {}", ra.path))?;

    // argv[0] is the path as invoked, then whatever followed. Under a sysroot
    // that host path may mean nothing to the guest, so it can be overridden.
    let mut argv = vec![ra.argv0.clone().unwrap_or_else(|| ra.path.clone())];
    argv.append(&mut ra.guest_args);
    ra.opts.args = argv;
    // A client finds the display by joining these two together, so a guest
    // given a compositor and not told where it is would never connect. Only
    // defaulted: a caller that set either of them meant it.
    if let Some(path) = ra.opts.wayland.clone() {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        for (k, v) in [("XDG_RUNTIME_DIR", dir), ("WAYLAND_DISPLAY", name)] {
            if !ra.opts.env.iter().any(|e| e.starts_with(&format!("{k}="))) {
                ra.opts.env.push(format!("{k}={v}"));
            }
        }
    }
    if ra.opts.env.is_empty() {
        ra.opts.env = vec!["PATH=/usr/bin:/bin".into(), "HOME=/root".into()];
    }

    ra.opts.jit = ra.jit;
    let (exit, _stats) = gex_native::run_program(&program, &ra.opts, &ra.xlate)?;
    match &exit {
        Exit::Status(c) => Ok(ExitCode::from((*c & 0xff) as u8)),
        Exit::Signal {
            signal,
            pc,
            detail,
            internal,
        } => {
            // A process killed by an ordinary signal says nothing itself —
            // Linux prints nothing and the shell reports the status. Only a
            // gex limitation is worth a message, since nothing else would
            // explain it.
            if *internal {
                // Not a guest crash, whatever the signal number says. gex
                // delivers its own failures as SIGSEGV because that is the
                // only way to stop a running guest, and calling that
                // "terminated by signal 11" sent one investigation looking
                // for a miscompile when the real answer was a resource
                // limit. Say which side failed.
                eprintln!("gexc: gex failed at guest pc=0x{pc:x}: {detail}");
            } else if ra.opts.verbose {
                eprintln!("gexc: guest terminated by signal {signal} at pc=0x{pc:x}: {detail}");
            }
            Ok(ExitCode::from((exit.code() & 0xff) as u8))
        }
    }
}

/// `gexc seed`: one module that will run any program, given a runtime
/// translator behind it.
fn cmd_seed(args: &[String]) -> Result<()> {
    let mut out = None;
    let mut shared_memory = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                out = Some(args.get(i).context("-o needs a path")?.clone());
            }
            "--no-shared-memory" => shared_memory = false,
            other => bail!("unexpected argument {other}"),
        }
        i += 1;
    }
    let out = out.unwrap_or_else(|| "seed.wasm".to_string());
    let module = gex_xlate::seed_module(shared_memory);
    std::fs::write(&out, &module.wasm).with_context(|| format!("writing {out}"))?;
    gex_native::validate_module(&module.wasm)
        .with_context(|| format!("{out} is not valid wasm"))?;
    eprintln!("{out}: {} bytes, translates nothing", module.wasm.len());
    Ok(())
}

fn cmd_build(args: &[String]) -> Result<()> {
    let mut path = None;
    let mut out = None;
    let mut exhaustive = false;
    let mut shared_memory = true;
    let mut no_validate = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => {
                i += 1;
                out = Some(args.get(i).context("-o needs a path")?.clone());
            }
            // A built module has no runtime translator behind it, so anything
            // discovery misses is a dispatch miss with nowhere to go.
            "--exhaustive" => exhaustive = true,
            // A host that cannot import a shared memory -- a Rust shim built
            // without `+atomics`, which is every stable-toolchain build --
            // needs the guest module to declare a plain one.
            "--no-shared-memory" => shared_memory = false,
            // Only for looking at a module that is known not to validate.
            "--no-validate" => no_validate = true,
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected an input ELF")?;
    let out = out.unwrap_or_else(|| format!("{path}.wasm"));

    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    let image = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;
    let (module, stats) = gex_xlate::translate_image(
        &image,
        &Options {
            exhaustive,
            shared_memory,
            ..Options::default()
        },
    );
    std::fs::write(&out, &module.wasm).with_context(|| format!("writing {out}"))?;
    if !no_validate {
        gex_native::validate_module(&module.wasm)
            .with_context(|| format!("{out} is not valid wasm"))?;
    }
    eprintln!(
        "{out}: {} blocks, {} guest instructions, {} KiB wasm ({:.1}x guest \
         text, {:.0}% of it register traffic across block boundaries)",
        stats.blocks,
        stats.instructions,
        stats.wasm_bytes / 1024,
        stats.wasm_bytes as f64 / (stats.instructions.max(1) * 4) as f64,
        100.0 * gex_emit::tier1::CTX_BYTES.with(|c| c.get()) as f64 / stats.wasm_bytes as f64,
    );
    // Which registers make up that percentage, for anyone considering doing
    // something about it.
    if std::env::var_os("GEX_REG_HISTOGRAM").is_some() {
        let mut v: Vec<(String, usize)> = gex_emit::tier1::REG_TRAFFIC.with(|t| {
            t.borrow()
                .iter()
                .map(|(r, n)| (format!("{r:?}"), *n))
                .collect()
        });
        v.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let total: usize = v.iter().map(|(_, n)| n).sum::<usize>().max(1);
        let mut acc = 0;
        for (name, n) in v.iter().take(16) {
            acc += n;
            eprintln!(
                "{out}:   {name:<8} {n:>9} ({:.1}%, running {:.1}%)",
                100.0 * *n as f64 / total as f64,
                100.0 * acc as f64 / total as f64
            );
        }
    }
    let (regions, in_regions) = gex_emit::tier1::REGIONS.with(|c| c.get());
    if regions > 0 {
        eprintln!(
            "{out}: {regions} loops emitted as one function each, {in_regions} blocks in them"
        );
    }
    if stats.illegal_blocks > 0 {
        eprintln!(
            "{out}: {} blocks end in an instruction gex cannot execute yet; \
             reaching one raises SIGILL",
            stats.illegal_blocks
        );
    }
    Ok(())
}

fn cmd_disasm(args: &[String]) -> Result<()> {
    let path = args.first().context("expected an input ELF")?;
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let image = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;

    let mut symbols: Vec<_> = image.symbols.iter().collect();
    symbols.sort_by_key(|s| s.value);

    for seg in &image.segments {
        if !seg.executable {
            continue;
        }
        for (i, word) in seg.bytes.chunks_exact(4).enumerate() {
            let pc = seg.vaddr + (i * 4) as u64;
            if let Some(sym) = symbols.iter().find(|s| s.value == pc) {
                println!("\n{:016x} <{}>:", pc, sym.name);
            }
            let raw = u32::from_le_bytes(word.try_into().unwrap());
            let inst = gex_isa::decode(raw, pc);
            println!("{pc:8x}:\t{raw:08x}\t{}", gex_isa::fmt::disasm(&inst, pc));
        }
    }
    Ok(())
}

fn cmd_ir(args: &[String]) -> Result<()> {
    let mut path = None;
    let mut at = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--at" => {
                i += 1;
                let v = args.get(i).context("--at needs an address")?;
                let v = v.strip_prefix("0x").unwrap_or(v);
                at = Some(u64::from_str_radix(v, 16).context("--at expects hex")?);
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected an input ELF")?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    let image = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;
    let at = at.unwrap_or(image.entry);

    let view = gex_xlate::CodeView::new(&image);
    let block = gex_lift::lift_block(|pc| view.fetch(pc), at, gex_lift::BlockLimits::default());
    print!("{}", gex_ir::print::block(&block));
    Ok(())
}
