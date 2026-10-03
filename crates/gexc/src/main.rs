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

mod prof;
mod eject;
mod nt_run;
mod pe_info;

/// Where the dynamic linker is placed, well above a PIE executable so their
/// mappings cannot overlap. A closure module for `ld.so` is only correct at the
/// address the host's loader uses, and it uses this one.
const INTERP_BASE: u64 = gex_abi::INTERP_BASE;

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
        "run" => {
            let r = cmd_run(&argv[2..]);
            prof::finish();
            r
        }
        "build" => cmd_build(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "seed" => cmd_seed(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "closure" => cmd_closure(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "pe-info" => pe_info::cmd_pe_info(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "profile" => cmd_profile(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "eject" => match eject::cmd_eject(&argv[2..])? {
            Some(exit) => Ok(report_exit(&exit, false)),
            None => Ok(ExitCode::SUCCESS),
        },
        "disasm" => cmd_disasm(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "ir" => cmd_ir(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "funcs" => cmd_funcs(&argv[2..]),
        "hotset" => cmd_hotset(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "cgen-image" => cmd_cgen_image(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "cgen-c" => cmd_cgen_c(&argv[2..]).map(|_| ExitCode::SUCCESS),
        "cgen-prefix" => cmd_cgen_prefix(&argv[2..]).map(|_| ExitCode::SUCCESS),
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
                                   is not;
                                   --closure --sysroot DIR: translate the
                                   program *and* every object it reaches
                                   through DT_NEEDED, each at the base a
                                   pinned run places it, writing one module
                                   per object and the layout beside them)
  gexc seed   -o <out.wasm>        write a module that translates nothing, for
                                   a host that will translate at runtime and
                                   does not know the program in advance
  gexc closure <elf> --sysroot DIR
                                   what the program actually consists of:
                                   every object reached through DT_NEEDED,
                                   where it would be placed, and how much
                                   text a whole-program build would face
{}
  gexc profile merge -o OUT IN...  combine profiles (gexc run --record); objects
                                   match by content hash, so libc's counts from
                                   different programs add up (--split DIR:
                                   one file per object instead of -o)
  gexc profile show [--top N] IN   summarise a profile
{}
  gexc disasm <elf>                disassemble the executable segments
  gexc ir     <elf> --at <addr>    print the GIR for one basic block
  gexc run [--cgen|--no-cgen] <program.exe> ...
                                   a Windows program uses the function-tier
                                   units built for its images (Wine's at
                                   staging, the program's own on an earlier
                                   run); an image without one runs on tier-1
                                   and gets one built for the next run.
                                   --cgen: every program image must have one;
                                   --no-cgen: tier-1 only
  gexc cgen-image <pe> [--pe-base HEX] [--jobs N]
                                   build a PE image's function-tier unit (the
                                   whole image, as C, compiled and cached) for
                                   `gexc run --cgen`, which installs it when the
                                   NT layer maps the image at that base. Run it
                                   with a memory cap: recovery of a large image
                                   is the peak
  gexc cgen-prefix [PREFIX] [--set FILE] [--jobs N] [--layout-only]
                                   compile a staged Wine prefix's core DLLs
                                   (tools/nt-cgen-core.txt, or --set) for the
                                   function tier: writes PREFIX/gex-layout, the
                                   fixed base each one gets in every process,
                                   and builds each unit at that base.
                                   tools/nt-stage-wine.sh runs it
  gexc hotset <elf|pe> --samples FILE [--pe-base HEX] [--budget N] -o OUT
                                   the function tier's hot set for an image:
                                   the recovered functions holding the most
                                   sampled addresses (GEX_SAMPLE_OUT of a
                                   `--sample` run), then their direct callees,
                                   up to N (default 4000); for
                                   `gexc build --cgen --functions OUT`
  gexc funcs  <elf|pe> [--at ADDR] [--ir] [--ssa] [--check] [--run N] [--list] [--base HEX]
                                   [--vsyscall-guards] [--guards FILE] [--profile FILE] [--truth FILE]
                                   recover functions (CFGs, jump tables,
                                   call-graph summaries) and report them.
                                   --ir prints function-level GIR, --ssa in
                                   SSA form; --check verifies every function
                                   and round-trips its text form, and runs
                                   each N times (default 8) on the GIR
                                   interpreter: SSA against memory form, and
                                   its summary's claims

run options:
  --strace        trace every syscall on stderr
  --verbose       report translation statistics
  --env K=V       add an environment variable (repeatable)
  --sysroot DIR   present DIR as the guest's root filesystem
  --pin FILE      place each object where FILE says, as written by
                  `gexc closure --pins`, and report where they landed
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
  --exhaustive-for NAME[,NAME]
                  do that for these objects of a closure only. libc is what
                  every program shares, and a closure built for one program
                  holds only the libc paths that program walks
  --no-jit        no runtime translation; implies --exhaustive
  --cgen          the function tier: compile the program's blocks as C with
                  clang (needs clang and wasm-ld; docs/function-tier.md)
  --fjit          the function JIT: every function recovered at run time
                  from guest memory, a batch at a time, as wasm emitted
                  directly and installed in the function table
                  (docs/merge-plan.md §9.8); tier-1 runs what it does not
  --fjit-image    the function JIT ahead of time: the program image's
                  recovered functions spliced into its tier-1 module, as
                  --cgen does with clang
  --lazy          discover only from the entry point, leaving the rest to
                  runtime translation; much faster to start on a big binary
  --no-cache      do not reuse compiled modules between runs
  --record DIR    write an execution profile (DIR/profile.json) and a
                  session manifest (DIR/session.json) at exit: exact block
                  and call counts, top-4 indirect targets, unexpected
                  returns, interior entries, syscalls per function, the
                  objects loaded, files opened. GEX_PROFILE=1 does the same
                  into $GEX_PROFILE_OUT (default ./gex-profile). Formats:
                  docs/profile-format.md
  --cwd DIR       start in this guest directory
  --argv0 NAME    what the guest sees as argv[0]; useful with --sysroot,
                  where the host path the program was loaded from is not a
                  path the guest can resolve",
        pe_info::USAGE,
        eject::USAGE
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
    /// A Windows program's staged Wine prefix. See `nt_run`.
    wine_prefix: Option<String>,
    /// `--no-cgen`: a Windows program runs on tier-1 only, prebuilt units or
    /// not (`gex_native::nt_cgen::Mode`).
    no_cgen: bool,
}

fn parse_run_args(args: &[String]) -> Result<RunArgs> {
    let mut path = None;
    let mut guest_args = Vec::new();
    let mut opts = RunOptions::default();
    // Discovery is best-effort; runtime translation fills the gaps.
    // `exhaustive` is the fallback when that is off.
    let mut exhaustive = false;
    let mut no_jit = false;
    let mut lazy = false;
    let mut cgen = false;
    let mut fjit = false;
    let mut fjit_image = false;
    let mut no_cgen = false;
    let mut argv0: Option<String> = None;
    let mut wine_prefix: Option<String> = None;
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
            // A layout written by `gexc closure --pins`: every object is placed
            // exactly where the file says, so a module built against those
            // addresses is correct.
            "--pin" => {
                i += 1;
                let v = args.get(i).context("--pin needs a manifest path")?;
                let text = std::fs::read_to_string(v).with_context(|| format!("reading {v}"))?;
                for line in text.lines() {
                    // `base span path`. The span keeps the placer from handing
                    // out an address inside a pinned object.
                    let mut f = line.splitn(3, ' ');
                    let (Some(base), Some(_span), Some(path)) = (f.next(), f.next(), f.next())
                    else {
                        continue;
                    };
                    let base = base.trim().trim_start_matches("0x");
                    let Ok(base) = u64::from_str_radix(base, 16) else {
                        continue;
                    };
                    opts.pins.insert(path.trim().to_string(), base);
                }
                opts.report_pins = true;
            }
            "--no-jit" => no_jit = true,
            "--lazy" => lazy = true,
            "--cgen" => cgen = true,
            "--fjit" => fjit = true,
            "--fjit-image" => fjit_image = true,
            "--no-cgen" => no_cgen = true,
            "--no-cache" => opts.cache = false,
            "--cwd" => {
                i += 1;
                let v = args.get(i).context("--cwd needs a directory")?;
                opts.cwd = Some(v.clone());
            }
            "--record" => {
                i += 1;
                let v = args.get(i).context("--record needs a directory")?;
                opts.record = Some(v.into());
            }
            "--argv0" => {
                i += 1;
                let v = args.get(i).context("--argv0 needs a name")?;
                argv0 = Some(v.clone());
            }
            "--wine-prefix" => {
                i += 1;
                wine_prefix = Some(args.get(i).context("--wine-prefix needs a directory")?.clone());
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
    // `GEX_PROFILE=1` is `--record $GEX_PROFILE_OUT`.
    if opts.record.is_none()
        && std::env::var("GEX_PROFILE").is_ok_and(|v| !v.is_empty() && v != "0")
    {
        opts.record = Some(
            std::env::var("GEX_PROFILE_OUT")
                .unwrap_or_else(|_| "gex-profile".to_string())
                .into(),
        );
    }
    // The function tier's C code carries no profiling instrumentation: a
    // recorded run would lose every count in it without a word.
    if (cgen || fjit || fjit_image) && opts.record.is_some() {
        bail!(
            "--cgen cannot be combined with --record or GEX_PROFILE=1: the function tier's \
             C code is not instrumented, so the profile would silently miss everything it runs \
             (record with tier-1, then build with --cgen)"
        );
    }
    Ok(RunArgs {
        path,
        argv0,
        guest_args,
        opts,
        xlate: Options {
            exhaustive: exhaustive || no_jit,
            lazy: lazy && !no_jit,
            cgen,
            fjit: fjit_image,
            no_blocks: fjit,
            ..Options::default()
        },
        jit: !no_jit,
        wine_prefix,
        no_cgen,
    })
}

/// Hex addresses, one per line (`#` starts a comment).
fn read_addr_list(path: &str) -> Result<Vec<u64>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let w = line.split('#').next().unwrap_or("").split_whitespace().next();
        if let Some(w) = w {
            let v = u64::from_str_radix(w.trim_start_matches("0x"), 16)
                .with_context(|| format!("{path}:{}: not a hex address: {w}", n + 1))?;
            out.push(v);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// `gexc build` on a PE image (an exe or DLL): placed at `pe_base` (its
/// preferred base by default) and translated as one image module, the way
/// the NT layer would map it -- tier-1, or the function tier with `--cgen`.
#[allow(clippy::too_many_arguments)]
fn build_pe_image(
    path: &str,
    bytes: &[u8],
    out: &str,
    pe_base: Option<u64>,
    exhaustive: bool,
    shared_memory: bool,
    cgen: bool,
    no_validate: bool,
    functions: Option<Vec<u64>>,
) -> Result<()> {
    let pe = gex_ld::pe::parse(bytes).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let base = pe_base.unwrap_or(pe.preferred_base);
    let (image, seeds) = gex_xlate::func::pe_input(&pe, base).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    gex_xlate::set_compat(image.class32);
    let t0 = std::time::Instant::now();
    let (module, stats) = gex_xlate::translate_pe_image(
        &image,
        &seeds,
        &match functions {
            // A hot set (`--functions`): tier-1 stubs at those entries only,
            // and the function tier for exactly those functions. The rest of
            // the image stays with the runtime translator.
            Some(list) => Options {
                shared_memory,
                cgen,
                lazy: true,
                seeds_only: true,
                extra_entries: list,
                ..Options::default()
            },
            None => Options {
                exhaustive,
                shared_memory,
                cgen,
                ..Options::default()
            },
        },
    );
    std::fs::write(out, &module.wasm).with_context(|| format!("writing {out}"))?;
    if !no_validate {
        gex_native::validate_module(&module.wasm).with_context(|| format!("{out} is not valid wasm"))?;
    }
    eprintln!(
        "{out}: PE image at {base:#x}: {} blocks, {} guest instructions, {} KiB wasm, {:.1}s",
        stats.blocks,
        stats.instructions,
        stats.wasm_bytes / 1024,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The memcpy/memmove/memset/mempcpy definitions in `image`'s own text, by
/// address (`Symbol::value` is already biased), for `Options::intrinsics`:
/// a plain build and `gexc run` serve them with the host builtins, as closure
/// builds and the browser's runtime translator do, so every path agrees on
/// what those functions are.
///
/// Not for 32-bit (i386) images: `intrinsic::block` reads the 64-bit
/// register arguments and pops an 8-byte return address, while i386 cdecl
/// passes them on the stack.
fn image_intrinsics(image: &gex_ld::Image) -> std::collections::HashMap<u64, gex_xlate::Intrinsic> {
    if image.class32 {
        return Default::default();
    }
    let (tlo, thi) = image.text_range;
    image
        .symbols
        .iter()
        .filter(|s| !s.ifunc && s.value >= tlo && s.value < thi)
        .filter_map(|s| gex_xlate::intrinsic::recognise(&s.name).map(|k| (s.value, k)))
        .collect()
}

/// `gexc run --fjit`: the function JIT serves the function table's misses.
struct FjitTranslator(gex_xlate::fjit::Jit);

impl gex_kernel::ftab::Translator for FjitTranslator {
    fn translate(&self, req: &gex_kernel::ftab::Request<'_>) -> Option<gex_kernel::ftab::Translated> {
        self.0
            .translate(req.mem, req.addr, req.executable, req.read_only, req.page_gen, req.class32)
            .map(|wasm| gex_kernel::ftab::Translated { wasm })
    }
}

fn cmd_run(args: &[String]) -> Result<ExitCode> {
    prof::start();
    let mut ra = parse_run_args(args)?;
    let bytes = std::fs::read(&ra.path).with_context(|| format!("reading {}", ra.path))?;
    if gex_ld::pe::is_pe(&bytes) {
        // A Windows program: Wine's PE DLLs on gex's NT layer.
        if ra.opts.record.is_some() {
            bail!("--record/GEX_PROFILE: profiling a Windows program is not supported yet");
        }
        // The function tier for the images the NT layer maps
        // (gex_native::nt_cgen, docs/function-tier.md §11): prebuilt units
        // are used where they exist, and an image without one runs on tier-1
        // while its unit is built for the next run; --cgen requires every
        // program image to have one; --no-cgen is tier-1 only.
        use gex_native::nt_cgen::Mode;
        if ra.xlate.no_blocks {
            gex_kernel::ftab::set_translator(Box::new(FjitTranslator(gex_xlate::fjit::Jit::new(Default::default()))));
        }
        gex_native::nt_cgen::set_mode(if ra.no_cgen || ra.xlate.no_blocks {
            Mode::Off
        } else if ra.xlate.cgen {
            Mode::Require
        } else {
            Mode::Default
        });
        let exit = nt_run::run(&ra, &bytes)?;
        return Ok(report_exit(&exit, ra.opts.verbose));
    }
    // Resolved through the guest's namespace (the sysroot when given), so a
    // program links against its own root's libraries, not the host's.
    let sysroot = ra.opts.sysroot.clone();
    let program = gex_ld::load_program(&bytes, gex_abi::PIE_BASE, INTERP_BASE, &|guest_path| {
        let host = match &sysroot {
            Some(root) => root.join(guest_path.trim_start_matches('/')),
            None => std::path::PathBuf::from(guest_path),
        };
        std::fs::read(host).ok()
    })
    .with_context(|| format!("loading {}", ra.path))?;
    ra.xlate.intrinsics = image_intrinsics(&program.exe);
    if let Some(i) = &program.interp {
        ra.xlate.intrinsics.extend(image_intrinsics(i));
    }
    if ra.xlate.no_blocks {
        let jit = gex_xlate::fjit::Jit::new(ra.xlate.intrinsics.clone());
        jit.add_image(&program.exe, Some(&bytes));
        gex_kernel::ftab::set_translator(Box::new(FjitTranslator(jit)));
    }

    // argv[0] is the path as invoked, then whatever followed. Under a sysroot
    // that host path may mean nothing to the guest, so it can be overridden.
    let mut argv = vec![ra.argv0.clone().unwrap_or_else(|| ra.path.clone())];
    argv.append(&mut ra.guest_args);
    ra.opts.args = argv;
    // A client finds the display by joining these two, so default them when a
    // compositor is given. Only defaulted: a caller that set either meant it.
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
    if ra.opts.record.is_some() {
        gex_native::profile::set_exe_host_path(ra.path.clone().into());
    }
    let (exit, _stats) = gex_native::run_program(&program, &ra.opts, &ra.xlate)?;
    if let Some(dir) = gex_native::profile::finish(&exit)? {
        if ra.opts.verbose {
            eprintln!("[gex] profile and session manifest written to {}", dir.display());
        }
    }
    Ok(report_exit(&exit, ra.opts.verbose))
}

/// The exit status a shell sees for a guest's exit, with a message when gex
/// itself failed.
fn report_exit(exit: &Exit, verbose: bool) -> ExitCode {
    match exit {
        Exit::Status(c) => ExitCode::from((*c & 0xff) as u8),
        Exit::Signal {
            signal,
            pc,
            detail,
            internal,
        } => {
            // An ordinary signal says nothing itself, as on Linux. Only a gex
            // limitation is worth a message.
            if *internal {
                // Not a guest crash: gex delivers its own failures as SIGSEGV
                // (the only way to stop a running guest), so say which side
                // failed rather than let it read as "terminated by signal 11".
                eprintln!("gexc: gex failed at guest pc=0x{pc:x}: {detail}");
            } else if verbose {
                eprintln!("gexc: guest terminated by signal {signal} at pc=0x{pc:x}: {detail}");
            }
            ExitCode::from((exit.code() & 0xff) as u8)
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
    let mut exhaustive_for: Vec<String> = Vec::new();
    let mut shared_memory = true;
    let mut no_validate = false;
    let mut closure = false;
    // Emit the program's image as a `Shared` module rather than the one `Own`
    // module that declares the memory and tables. An `Own` image module has to
    // replace the seed, so every process instantiates it whether or not it runs
    // that program (a forked child about to `execve` pays to instantiate a
    // whole shell it never enters). Built `Shared`, it is just another deferred
    // library, instantiated by the first branch that lands in it.
    let mut shared_image = false;
    let mut also: Vec<String> = Vec::new();
    let mut sysroot: Option<String> = None;
    let mut profile: Vec<u64> = Vec::new();
    let mut cgen = false;
    let mut fjit = false;
    let mut pe_base: Option<u64> = None;
    let mut functions: Option<Vec<u64>> = None;
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
            // Exhaustive for *these* objects only. `--exhaustive` on a whole
            // closure is unusable (apt's is 678 MB against 82). Reachable-only
            // is the default since the browser lifts what the build missed, but
            // "reachable" means reachable from *this* program -- libc and ld.so
            // are shared by everything, so those are worth having whole.
            "--exhaustive-for" => {
                i += 1;
                exhaustive_for = args
                    .get(i)
                    .context("--exhaustive-for needs a comma-separated list of object names")?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            // A host that cannot import a shared memory (a shim built without
            // `+atomics`, i.e. every stable-toolchain build) needs a plain one.
            "--functions" => {
                i += 1;
                let p = args.get(i).context("--functions needs a file of hex addresses")?;
                functions = Some(read_addr_list(p)?);
            }
            "--pe-base" => {
                i += 1;
                let v = args.get(i).context("--pe-base needs an address")?;
                pe_base = Some(u64::from_str_radix(v.trim_start_matches("0x"), 16).context("--pe-base expects hex")?);
            }
            "--no-shared-memory" => shared_memory = false,
            // Only for looking at a module that is known not to validate.
            "--no-validate" => no_validate = true,
            // Translate the whole program: the executable and every DT_NEEDED
            // object, each at its pinned base. Without this a build translates
            // only the image handed to it, which is usually the wrapper.
            "--closure" => closure = true,
            // Another executable whose libraries belong in this closure.
            // Repeatable; `--with-dir` for a directory of them.
            "--with" => {
                i += 1;
                also.push(args.get(i).context("--with needs a path")?.clone());
            }
            "--with-dir" => {
                i += 1;
                let d = args.get(i).context("--with-dir needs a directory")?;
                let mut found: Vec<String> = std::fs::read_dir(d)
                    .with_context(|| format!("reading {d}"))?
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .collect();
                found.sort();
                also.extend(found);
            }
            "--shared-image" => shared_image = true,
            // The function tier: the blocks as C, through clang, spliced into
            // the module (docs/function-tier.md).
            "--cgen" => cgen = true,
            // The function JIT: recovered functions straight to wasm, no
            // clang (gex-fjit, docs/merge-plan.md §9.8).
            "--fjit" => fjit = true,
            "--sysroot" => {
                i += 1;
                sysroot = Some(args.get(i).context("--sysroot needs a directory")?.clone());
            }
            // A profile from a sandbox run (`GEX_COVERAGE_OUT`): one hex block PC
            // per line, the exact set a real run reached. Seeds each object's
            // translation with the PCs in its range instead of scanning the whole
            // symbol table, so the closure covers only reached code (and the
            // untaken branches off it, plus each touched page completed) --
            // ~15x smaller than static reachability and, because it carries the
            // indirect targets a static scan misses, correct without any partial
            // page to demand-fill.
            "--profile" => {
                i += 1;
                let p = args.get(i).context("--profile needs a path")?;
                let text = std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?;
                profile = text
                    .lines()
                    .filter_map(|l| {
                        let l = l.trim();
                        let l = l.strip_prefix("0x").unwrap_or(l);
                        u64::from_str_radix(l, 16).ok()
                    })
                    .collect();
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected an input ELF")?;
    let out = out.unwrap_or_else(|| format!("{path}.wasm"));

    if fjit && (closure || gex_ld::pe::is_pe(&std::fs::read(&path).unwrap_or_default())) {
        bail!("--fjit builds an ELF image only (closures and PE images: not yet)");
    }
    if closure {
        if cgen {
            bail!("--cgen does not handle --closure builds yet");
        }
        return build_closure(
            &path, &also, sysroot.as_deref(), &out, exhaustive, &exhaustive_for, shared_memory,
            no_validate, shared_image, &profile,
        );
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    if gex_ld::pe::is_pe(&bytes) {
        return build_pe_image(&path, &bytes, &out, pe_base, exhaustive, shared_memory, cgen, no_validate, functions);
    }
    let image = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;
    // A 32-bit (i386) program's code must be decoded in compatibility mode,
    // exactly as the runtime path does before translating (`run_program_with`).
    gex_xlate::set_compat(image.class32);
    let (module, stats) = gex_xlate::translate_image(
        &image,
        &Options {
            exhaustive,
            shared_memory,
            cgen,
            fjit,
            intrinsics: image_intrinsics(&image),
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
        100.0 * gex_emit::tier1::CTX_BYTES.load(std::sync::atomic::Ordering::Relaxed) as f64
            / stats.wasm_bytes as f64,
    );
    // What a block boundary costs, split three ways: which of the three to
    // attack is not obvious from the total.
    {
        use std::sync::atomic::Ordering::Relaxed;
        let ctx = gex_emit::tier1::CTX_BYTES.load(Relaxed);
        let sp = gex_emit::tier1::SAFEPOINT_BYTES.load(Relaxed);
        let disp = gex_emit::tier1::DISPATCH_BYTES.load(Relaxed);
        let guard = gex_emit::tier1::GUARD_BYTES.load(Relaxed);
        let total = stats.wasm_bytes.max(1);
        eprintln!(
            "  boundary: {:.0}% registers, {:.0}% safepoints, {:.0}% dispatch \
             ({:.0}% together, {} bytes over {} blocks)",
            100.0 * ctx as f64 / total as f64,
            100.0 * sp as f64 / total as f64,
            100.0 * disp as f64 / total as f64,
            100.0 * (ctx + sp + disp) as f64 / total as f64,
            ctx + sp + disp,
            stats.blocks,
        );
        eprintln!(
            "  bounds guards: {:.0}% ({} bytes)",
            100.0 * guard as f64 / total as f64,
            guard,
        );
    }
    // Which registers make up that percentage.
    if std::env::var_os("GEX_REG_HISTOGRAM").is_some() {
        let mut v: Vec<(String, usize)> = gex_emit::tier1::REG_TRAFFIC
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .map(|(r, n)| (format!("{r:?}"), *n))
            .collect();
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
    let (regions, in_regions) = *gex_emit::tier1::REGIONS.lock().unwrap();
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

/// Translate a program and every object it links against.
///
/// One module per object, placed where a pinned run will: the executable
/// declares the memory and dispatch table, every library imports them.
/// `out.wasm` is the executable, `out.NN.name.wasm` each library, `out.pins`
/// the layout they were built for. That layout must travel with the modules --
/// a run given any other is running code compiled for addresses it is not at.
fn build_closure(
    path: &str,
    also: &[String],
    sysroot: Option<&str>,
    out: &str,
    exhaustive: bool,
    exhaustive_for: &[String],
    shared_memory: bool,
    no_validate: bool,
    shared_image: bool,
    profile: &[u64],
) -> Result<()> {
    let (closure, interp) = closure_of_all(path, also, sysroot)?;
    // One decoding mode for the whole closure, taken from the executable: a
    // 32-bit (i386) program is 32-bit throughout, and a closure that mixes
    // classes cannot be translated in one mode.
    let class32 = closure.objects.first().map(|o| o.image.class32).unwrap_or(false);
    if let Some(o) = closure.objects.iter().find(|o| o.image.class32 != class32) {
        anyhow::bail!(
            "{}: a {}-bit object in a {}-bit program's closure",
            o.path,
            if o.image.class32 { 32 } else { 64 },
            if class32 { 32 } else { 64 }
        );
    }
    gex_xlate::set_compat(class32);
    // Sorted once so each object can binary-search its slice of reached PCs.
    let mut profile_sorted = profile.to_vec();
    profile_sorted.sort_unstable();
    profile_sorted.dedup();
    if !profile_sorted.is_empty() {
        eprintln!("gexc: profile-guided build from {} reached block PCs", profile_sorted.len());
    }
    for (name, asker) in &closure.missing {
        eprintln!("gexc: {name} not found, needed by {asker} -- the build will be incomplete");
    }
    let stem = out.strip_suffix(".wasm").unwrap_or(out);
    let mut pins = String::new();
    let mut images = String::new();
    let (mut blocks, mut insts, mut bytes_total) = (0usize, 0usize, 0usize);

    // Where the next object's slots start. All objects share one dispatch
    // table in one process, so they are numbered as if by one allocator even
    // though translated one at a time.
    let mut first_slot = 0u64;
    // Lowest slots to the objects the most processes will enter. A process must
    // grow its table to the *highest* slot it touches, and `Table.grow` fills
    // each new slot with the miss stub, so the cost is the highest index, not
    // the count used. In-degree (how many objects name it in `DT_NEEDED`) is
    // the proxy for how many processes reach it; libc is named by everything.
    // Ties keep breadth-first order so a rebuild does not reshuffle slots.
    // Only numbering changes here; addresses are pinned separately.
    let order: Vec<usize> = {
        let mut in_degree = vec![0usize; closure.objects.len()];
        for o in &closure.objects {
            for need in &o.image.needed {
                if let Some(i) = closure
                    .objects
                    .iter()
                    .position(|c| c.name == *need || c.path.ends_with(need.as_str()))
                {
                    in_degree[i] += 1;
                }
            }
        }
        let mut idx: Vec<usize> = (0..closure.objects.len()).collect();
        // Without `--shared-image` the first object owns the memory and the
        // tables, and that one cannot move.
        let fixed = if shared_image { 0 } else { 1 };
        idx[fixed..].sort_by_key(|&i| (std::cmp::Reverse(in_degree[i]), i));
        idx
    };
    let ordered: Vec<&gex_ld::closure::Object> =
        order.iter().map(|&i| &closure.objects[i]).collect();
    // Objects to leave OUT of the closure entirely, by name prefix (env
    // `GEX_SKIP`, comma-separated). They carry no module and are translated at
    // run time by the miss path -- the only correct path for a library whose
    // internal functions are reached through pointers installed at run time
    // (libgcrypt's Ed25519 field ops: static, so not in a stripped `.dynsym`,
    // and set in a heap `mpi_ec_ctx`, so no static pointer scan finds them). A
    // closure that covers such a library only partially leaves those blocks to
    // a demand-fill *into a closure-covered page*, which corrupts dispatch and
    // is what killed `gpgv` verifying an EdDSA `InRelease` signature. Skipped
    // whole, the library runs on the plain translator the way `?closure=0`
    // does, which is correct. The pin is still emitted so it loads where its
    // pinned neighbours expect.
    let skip: Vec<String> = std::env::var("GEX_SKIP")
        .ok()
        .map(|s| s.split(',').filter(|w| !w.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    // Objects NOT to fill the gaps of (`complete_pages`), by name prefix (env
    // `GEX_NO_COMPLETE`). Completion is on by default for every object: a
    // partly-covered page demand-fills its gaps at run time into slots the
    // closure already holds, and that partial-page dispatch corrupts -- apt's
    // gpgv method smashed its stack there, its forked gpgv aborted on a
    // `lock ABI version mismatch`, every process in an `apt-get update` a
    // different symptom of the same thing. The block discovery misses is a C++
    // exception landing pad (reached by neither a branch nor a pointer) or a
    // cold path, and the only cure is to carry it.
    //
    // The exception is a big, SPARSELY-reached math library: completing it is
    // effectively its exhaustive size (libgcrypt: 300 MB), and it does not need
    // it -- libgcrypt is already correct through the code-pointer scan, and the
    // GnuTLS math stack (nettle/gmp/hogweed/gnutls) an http `apt-get update`
    // never enters. Everything else -- libstdc++, libgcc_s, libgpg-error, the
    // apt libraries, libseccomp, the image roots -- is carried whole-on-its-
    // touched-pages so no live process demand-fills into a partial page.
    let no_complete: Vec<String> = std::env::var("GEX_NO_COMPLETE")
        .ok()
        .map(|s| s.split(',').filter(|w| !w.is_empty()).map(str::to_string).collect())
        .unwrap_or_else(|| {
            ["libgcrypt", "libnettle", "libgmp", "libhogweed", "libgnutls"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    // Every object's `memcpy`/`memmove`/`memset`/`mempcpy`, by biased address,
    // so `translate_image` emits the shim's intrinsic for them instead of
    // lifting glibc's hand-written SIMD -- the same substitution the runtime
    // translator does. Without it the closure and the runtime disagree: a run
    // on a closure lifts and executes glibc's own `memcpy`, and a wrong lane in
    // that translation is a wild copy (the heap corruption `gpgv` hit verifying
    // an EdDSA signature only ever appeared with a closure). Global, because a
    // call resolves to another object's definition; keyed by biased address,
    // which is where the block for that function is discovered.
    let intrinsics = closure_intrinsics(&closure.objects);
    for (n, o) in ordered.into_iter().enumerate() {
        let skipped = skip.iter().any(|w| o.name.starts_with(w.as_str()));
        // This object's slice of the reached-PC profile, when one was given. The
        // profile holds absolute biased PCs and every object loads at the same
        // pinned base the run used, so a range test on the object's text picks
        // out its reached blocks.
        let seeds: Vec<u64> = if profile_sorted.is_empty() {
            Vec::new()
        } else {
            let (tlo, thi) = o.image.text_range;
            let lo = profile_sorted.partition_point(|&p| p < tlo);
            let hi = profile_sorted.partition_point(|&p| p < thi);
            profile_sorted[lo..hi].to_vec()
        };
        let guided = !profile_sorted.is_empty();
        if guided {
            eprintln!("gexc:   {} <- {} reached PCs", o.name, seeds.len());
        }
        // Whole, for the objects named. Matched on the start of the name so
        // `libc.so.6` catches `libc.so.6` and `libc` catches it too. A profile
        // overrides it: the reached set already carries what a run of this
        // program touches, and keeping a library whole under a profile just
        // re-inflates the closure the profile exists to shrink.
        let whole = !guided
            && (exhaustive || exhaustive_for.iter().any(|w| o.name.starts_with(w.as_str())));
        // Fill the gaps on this object's touched pages (C++ landing pads and
        // cold paths), unless it is already whole (nothing left to fill) or on
        // the no-complete list (a sparse math library not worth its exhaustive
        // size). Root images included -- an apt method wraps its `main` in a
        // try/catch, so the landing pad the unwinder jumps to lives in the
        // program's own image. Under a profile, always complete: a run reaches
        // few pages, completing them is cheap, and it is what guarantees no
        // reached page comes out partial (the closure's one corruption hazard).
        let complete = guided
            || (!whole && !no_complete.iter().any(|w| o.name.starts_with(w.as_str())));
        let opts = Options {
            first_slot,
            exhaustive: whole,
            // Under a profile, seed discovery from only the reached PCs and skip
            // the symbol-table and pointer scans -- that is the whole trim. The
            // executable's real entry is still added inside `translate_image`.
            lazy: guided,
            scan_pointers: !guided,
            // The seeds are the reached set; do not follow their branches onto
            // fresh pages (that re-inflates to static reachability). complete_pages
            // still makes every touched page whole, so the closure is the reached
            // pages exactly.
            seeds_only: guided,
            extra_entries: seeds,
            // `complete_pages` fills the gaps on any page a block already
            // touches, so a partly-covered page becomes whole and nothing on it
            // demand-fills at run time into slots the closure already holds --
            // the partial-page corruption that smashed apt's gpgv method's
            // stack (a C++ exception landing pad, reached by neither branch nor
            // pointer, is the block discovery misses). Per object (`complete_for`
            // above), since completing a big C library is its exhaustive size for
            // gaps no path ever takes. The old cascade is gone: completion blocks
            // are lifted but their successors are not followed onto fresh pages.
            complete_pages: complete,
            intrinsics: intrinsics.clone(),
            shared_memory,
            // On for every object regardless of size: slots are numbered in one
            // sequence, so a small object's pages can sit far into table 0 and
            // otherwise make every process grow the table that far up front.
            demand_fill: true,
            // The first object is the executable and owns the address space,
            // unless `--shared-image` gives that to a library instead.
            linkage: if n == 0 && !shared_image {
                gex_xlate::Linkage::Own
            } else {
                gex_xlate::Linkage::Shared
            },
            ..Options::default()
        };
        // Apply the object's own RELATIVE relocations before translating, so
        // `scan_pointers` follows a relocated dispatch pointer into the code it
        // names -- libgcrypt's EdDSA verify is reached only through such a
        // pointer, and unlinked here it read as an unbiased addend and was
        // never lifted. A clone because the closure's objects are shared and
        // only the translation needs the relocated data. See
        // `gex_ld::elf::apply_own_relative`.
        let mut img = o.image.clone();
        gex_ld::elf::apply_own_relative(&mut img);
        // An object with more blocks than fit in one wasm module (libQt6Gui,
        // Mesa) comes back as several parts; the runtime splitter's logic, now
        // reachable at build time. All the closure's objects are `Shared` under
        // `--shared-image`, which is what makes the split valid (an `Own` image
        // module cannot be split, and executables are small enough not to need
        // it anyway).
        let (modules, stats) =
            gex_xlate::translate_image_split(&img, &opts, gex_xlate::MAX_FUNCS_PER_MODULE);
        // Named like a library when it is one, so a manifest that collects
        // `c.NN.*.wasm` picks it up and the loader has nothing special to do.
        // Multiple parts are `.p0`, `.p1`, … under the same object index; the
        // glob takes them all, and they sort in build (page) order.
        let base = if n == 0 && !shared_image {
            stem.to_string()
        } else {
            format!("{stem}.{n:02}.{}", o.name)
        };
        let part_file = |i: usize| -> String {
            if modules.len() == 1 {
                format!("{base}.wasm")
            } else {
                format!("{base}.p{i}.wasm")
            }
        };
        // A skipped object is translated only to advance `first_slot`, so every
        // OTHER object keeps the exact slots it has without the skip -- moving
        // that layout exposed a latent slot bug (apt-get's startup died). Its
        // module is NOT written: with no module the library runs on the plain
        // translator, the correct path for a library whose internals are
        // reached through pointers installed at run time (libgcrypt's Ed25519
        // field ops), which build-time discovery cannot see and a partial
        // closure page then demand-fills wrong. The pin still goes out, so it
        // loads where its pinned neighbours expect and its own slots -- left
        // empty in the closure -- are numbered off the plain translator.
        if !skipped {
            for (i, module) in modules.iter().enumerate() {
                let file = part_file(i);
                std::fs::write(&file, &module.wasm).with_context(|| format!("writing {file}"))?;
                if !no_validate {
                    gex_native::validate_module(&module.wasm)
                        .with_context(|| format!("{file} is not valid wasm"))?;
                }
            }
        }
        // Neither the interpreter nor a root image is pinned: gex's loader
        // places both (ld.so at INTERP_BASE, roots at PIE_BASE), so there is no
        // mmap reservation to catch. `depth == 0` marks a root.
        if o.depth > 0 && Some(o.path.as_str()) != interp.as_deref() && !is_interp_name(&o.name) {
            pins.push_str(&format!("{:#x} {:#x} {}\n", o.image.bias, o.span, o.path));
        }
        // And which file holds which program's image, so the loader can tell
        // a process the pages at `PIE_BASE` are or are not its own. A root image
        // is `Own`/small, so it is always one part.
        if o.depth == 0 {
            let f = part_file(0);
            images.push_str(&format!("{} {}\n", f.rsplit('/').next().unwrap_or(&f), o.path));
        }
        // The LAST part carries the cumulative page table (the parts are
        // threaded), so its `next_slot` is where the next object begins.
        first_slot = modules
            .last()
            .map_or(first_slot, |m| m.metadata().next_slot);
        if !skipped {
            blocks += stats.blocks;
            insts += stats.instructions;
            bytes_total += stats.wasm_bytes;
        }
        eprintln!(
            "  {:<40} {:>7} blocks {:>7} KiB  at {:#x}{}",
            o.name,
            stats.blocks,
            stats.wasm_bytes / 1024,
            o.image.bias,
            if skipped {
                "  (slots reserved; runtime-translated)".to_string()
            } else if modules.len() > 1 {
                format!("  ({} parts)", modules.len())
            } else {
                String::new()
            },
        );
    }
    let pin_file = format!("{stem}.pins");
    std::fs::write(&pin_file, pins).with_context(|| format!("writing {pin_file}"))?;
    let image_file = format!("{stem}.images");
    std::fs::write(&image_file, images).with_context(|| format!("writing {image_file}"))?;
    eprintln!(
        "{} objects: {} blocks, {} guest instructions, {} KiB wasm; layout in {}",
        closure.objects.len(),
        blocks,
        insts,
        bytes_total / 1024,
        pin_file,
    );
    Ok(())
}

/// Every object's `mem*` functions, by placed address. See `build_closure`.
fn closure_intrinsics(
    objects: &[gex_ld::closure::Object],
) -> std::collections::HashMap<u64, gex_xlate::Intrinsic> {
    let mut intrinsics = std::collections::HashMap::new();
    for o in objects {
        // Not for 32-bit (i386) objects, as `image_intrinsics`: the
        // intrinsic block reads the 64-bit register arguments and pops an
        // 8-byte return address, and i386 cdecl passes them on the stack.
        if o.image.class32 {
            continue;
        }
        let (tlo, thi) = o.image.text_range;
        for s in &o.image.symbols {
            if s.ifunc {
                continue;
            }
            // Defined, and in this object's own text: an undefined/imported
            // `memcpy` has no address here (it resolves to libc's definition),
            // and the executables all share `PIE_BASE`, so an out-of-text entry
            // would collide across them. `value` is already biased: gex-ld
            // adds the load bias when it reads the symbol table.
            let addr = s.value;
            if addr < tlo || addr >= thi {
                continue;
            }
            if let Some(kind) = gex_xlate::intrinsic::recognise(&s.name) {
                intrinsics.insert(addr, kind);
            }
        }
    }
    intrinsics
}

/// Whether a file name is a dynamic linker.
///
/// By name rather than `PT_INTERP`, because they need not agree: dash's
/// `PT_INTERP` is `/lib/ld-linux-aarch64.so.1`, but libc's `DT_NEEDED` names
/// the same object `/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1`. Comparing
/// paths matched neither, so the interpreter was placed in the arena and
/// translated for an address it never occupies.
fn is_interp_name(name: &str) -> bool {
    name.starts_with("ld-linux") || name.starts_with("ld-musl") || name == "ld.so"
}

fn place_object(next: &mut u64, span: u64) -> u64 {
    const SLACK: u64 = 0x10000;
    let base = (*next + SLACK - 1) & !(SLACK - 1);
    *next = base + ((span + SLACK - 1) & !(SLACK - 1)) + SLACK;
    base
}

/// Resolve a program's closure with the standard placement.
fn closure_of(
    path: &str,
    sysroot: Option<&str>,
) -> Result<(gex_ld::closure::Closure, Option<String>)> {
    closure_of_all(path, &[], sysroot)
}

/// As [`closure_of`], plus the helpers the program forks.
///
/// A program is not only its executable: apt is `apt-get` plus the `file`,
/// `copy` and `store` methods it runs, which pull in objects `apt-get` does not.
/// The helpers contribute only their libraries, not their own text -- each is a
/// separate executable that would want the image module's base for itself.
fn closure_of_all(
    path: &str,
    also: &[String],
    sysroot: Option<&str>,
) -> Result<(gex_ld::closure::Closure, Option<String>)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {path}"))?;
    let root = sysroot.map(str::to_string);
    let read = move |p: &str| -> Option<Vec<u8>> {
        let r = root.as_deref()?;
        std::fs::read(format!(
            "{}/{}",
            r.trim_end_matches('/'),
            p.trim_start_matches('/')
        ))
        .ok()
    };
    let exe = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;
    let interp = exe.interp.clone();
    let guest_path = match sysroot {
        Some(r) => path.strip_prefix(r.trim_end_matches('/')).unwrap_or(path).to_string(),
        None => path.to_string(),
    };
    let mut next = gex_abi::MMAP_BOTTOM;
    // The interpreter is placed by gex's loader at `INTERP_BASE`, not by
    // `mmap`, so it is given that address here rather than a pinned one.
    let interp_for_place = interp.clone();
    let mut place = |path: &str, span: u64| {
        let name = path.rsplit('/').next().unwrap_or(path);
        if Some(path) == interp_for_place.as_deref() || is_interp_name(name) {
            return INTERP_BASE;
        }
        place_object(&mut next, span)
    };
    // The program first, so `objects[0]` is the executable, then helpers. A
    // helper that cannot be read is skipped with a word, not a failed build.
    let mut roots = vec![(exe, guest_path)];
    for h in also {
        let hb = match std::fs::read(h) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gexc: skipping helper {h}: {e}");
                continue;
            }
        };
        let img = match gex_ld::elf::load(&hb, gex_abi::PIE_BASE) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("gexc: skipping helper {h}: {e:?}");
                continue;
            }
        };
        let gp = match sysroot {
            Some(r) => h.strip_prefix(r.trim_end_matches('/')).unwrap_or(h).to_string(),
            None => h.clone(),
        };
        roots.push((img, gp));
    }
    Ok((
        gex_ld::closure::resolve_all(roots, &read, &mut place)?,
        interp,
    ))
}

/// Report the transitive `DT_NEEDED` closure of a program, without running it
/// and checkable against `ldd`.
fn cmd_closure(args: &[String]) -> Result<()> {
    let mut path = None;
    let mut sysroot: Option<String> = None;
    let mut pins_out: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--sysroot" => {
                i += 1;
                sysroot = Some(args.get(i).context("--sysroot needs a directory")?.clone());
            }
            "--pins" => {
                i += 1;
                pins_out = Some(args.get(i).context("--pins needs a path")?.clone());
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected an input ELF")?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    if gex_ld::pe::is_pe(&bytes) {
        // A Windows program: its closure is DLLs, found beside it.
        return pe_info::cmd_pe_info(args);
    }

    // Guest paths resolve inside the sysroot, as they would when the program
    // runs. Without one there is nowhere to look.
    let root = sysroot.clone();
    let read = move |p: &str| -> Option<Vec<u8>> {
        let r = root.as_deref()?;
        std::fs::read(format!(
            "{}/{}",
            r.trim_end_matches('/'),
            p.trim_start_matches('/')
        ))
        .ok()
    };

    let exe = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;
    // Placed *up* from the bottom of the arena, so pins stay clear of the
    // allocator, which fills down from the top. `ld.so` reserves the span plus
    // up to one max-alignment of slack, maps the segments MAP_FIXED inside, and
    // trims both ends; `place_object`'s gap covers that slack, or the pin's
    // address hint stops being free and is quietly ignored. The base is 64 KiB
    // aligned (the largest `p_align` here) so ld.so's rounding is a no-op and
    // the load bias is exactly the pinned address.
    let mut next = gex_abi::MMAP_BOTTOM;
    // The interpreter goes where the loader will put it, not in the arena.
    let mut place = |path: &str, span: u64| {
        if is_interp_name(path.rsplit('/').next().unwrap_or(path)) {
            return INTERP_BASE;
        }
        place_object(&mut next, span)
    };
    // What `$ORIGIN` is relative to: the executable's path *in the guest*,
    // which inside a sysroot is the host path with the prefix taken off.
    let guest_path = match &sysroot {
        Some(r) => path
            .strip_prefix(r.trim_end_matches('/'))
            .unwrap_or(&path)
            .to_string(),
        None => path.clone(),
    };
    let interp_path = exe.interp.clone();
    let closure = gex_ld::closure::resolve(exe, &guest_path, &read, &mut place)?;

    for o in &closure.objects {
        let (lo, hi) = o.image.text_range;
        println!(
            "{:>2}  {:<40} {:>8} KiB text  at 0x{:x}  span 0x{:x}",
            o.depth,
            o.name,
            (hi.saturating_sub(lo)) / 1024,
            o.image.bias,
            o.span,
        );
    }
    println!(
        "{} objects, {} KiB of text in total",
        closure.objects.len(),
        closure.text_bytes() / 1024,
    );
    // The manifest a run is given, so its layout matches this build's. One line
    // per object: base, then guest path. The executable and the interpreter are
    // left out -- gex's loader places both, so neither is pinned.
    if let Some(out) = &pins_out {
        let interp = interp_path.as_deref();
        let mut text = String::new();
        for o in closure.objects.iter().skip(1) {
            if Some(o.path.as_str()) == interp || is_interp_name(&o.name) {
                continue;
            }
            text.push_str(&format!("{:#x} {:#x} {}\n", o.image.bias, o.span, o.path));
        }
        std::fs::write(out, text).with_context(|| format!("writing {out}"))?;
        eprintln!("  pins written to {out}");
    }
    for (name, asker) in &closure.missing {
        eprintln!("  not found: {name}, needed by {asker}");
    }
    if sysroot.is_none() {
        eprintln!("  (no --sysroot, so nothing could be looked up)");
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
        let mut off = 0usize;
        while off < seg.bytes.len() {
            let pc = seg.vaddr + off as u64;
            if let Some(sym) = symbols.iter().find(|s| s.value == pc) {
                println!("\n{:016x} <{}>:", pc, sym.name);
            }
            let (text, len) = gex_xlate::arch::disasm(&seg.bytes[off..], pc);
            println!("{pc:8x}:\t{text}");
            off += len;
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
    if gex_ld::pe::is_pe(&bytes) {
        // A Windows program: its closure is DLLs, found beside it.
        return pe_info::cmd_pe_info(args);
    }
    let image = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;
    let at = at.unwrap_or(image.entry);

    let view = gex_xlate::CodeView::new(&image);
    let block = gex_xlate::lift_block(
        |pc, buf| view.fetch(pc, buf),
        at,
        gex_xlate::BlockLimits::default(),
    );
    print!("{}", gex_ir::print::block(&block));
    Ok(())
}

/// `gexc funcs`: function recovery, for inspection. See `gex_xlate::func`.
fn cmd_funcs(args: &[String]) -> Result<ExitCode> {
    let mut path = None;
    let mut at = None;
    let (mut ir, mut ssa, mut check, mut list) = (false, false, false, false);
    let mut show_uncovered = false;
    let mut show_unresolved = false;
    let mut runs: u64 = 8;
    let mut pe_base: Option<u64> = None;
    let mut truth: Option<String> = None;
    let mut vsyscall = false;
    let mut guard_file: Option<String> = None;
    let mut profile_file: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--at" => {
                i += 1;
                let v = args.get(i).context("--at needs an address")?;
                let v = v.strip_prefix("0x").unwrap_or(v);
                at = Some(u64::from_str_radix(v, 16).context("--at expects hex")?);
            }
            "--ir" => ir = true,
            "--ssa" => ssa = true,
            "--check" => check = true,
            "--list" => list = true,
            "--uncovered" => show_uncovered = true,
            "--unresolved" => show_unresolved = true,
            "--run" => {
                i += 1;
                runs = args.get(i).context("--run needs a count")?.parse().context("--run expects a number")?;
            }
            "--vsyscall-guards" => vsyscall = true,
            "--profile" => {
                i += 1;
                profile_file = Some(args.get(i).context("--profile needs a file")?.clone());
            }
            "--guards" => {
                i += 1;
                guard_file = Some(args.get(i).context("--guards needs a file")?.clone());
            }
            "--truth" => {
                i += 1;
                truth = Some(args.get(i).context("--truth needs a file")?.clone());
            }
            "--base" => {
                i += 1;
                let v = args.get(i).context("--base needs an address")?;
                pe_base = Some(u64::from_str_radix(v.trim_start_matches("0x"), 16).context("--base expects hex")?);
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected an input ELF")?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    let t0 = std::time::Instant::now();
    let (image, prog) = if gex_ld::pe::is_pe(&bytes) {
        let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        let base = pe_base.unwrap_or(pe.preferred_base);
        let (image, extra) = gex_xlate::func::pe_input(&pe, base).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        let prog = gex_xlate::func::recover_pe(&image, &extra, &Default::default());
        (image, prog)
    } else {
        let image = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;
        let prog = gex_xlate::func::recover(&image, Some(&bytes), &Default::default());
        (image, prog)
    };
    let t_rec = t0.elapsed();
    let t1 = std::time::Instant::now();
    // Guarded indirect calls (see gex_xlate::summary::Guards for the
    // contract a backend using them must keep).
    let mut guards = if vsyscall {
        gex_xlate::summary::vsyscall_guards(&prog)
    } else {
        Default::default()
    };
    if let Some(gf) = &guard_file {
        // One site per line: the calling instruction, then its targets.
        let text = std::fs::read_to_string(gf).with_context(|| format!("reading {gf}"))?;
        for line in text.lines() {
            let w: Vec<u64> = line
                .split_whitespace()
                .filter_map(|x| u64::from_str_radix(x.trim_start_matches("0x"), 16).ok())
                .collect();
            if w.len() >= 2 {
                guards.calls.insert(w[0], w[1..].to_vec());
            }
        }
    }
    if let Some(pf) = &profile_file {
        // Sites and targets the profile recorded for this file (S6's
        // format), named by the file's content hash.
        let text = std::fs::read_to_string(pf).with_context(|| format!("reading {pf}"))?;
        let profile = gex_xlate::profile::Profile::from_json(&text).map_err(|e| anyhow::anyhow!("{pf}: {e}"))?;
        let mut h = gex_cache::sha256::Sha256::new();
        h.update(&bytes);
        let id = format!("sha256:{}", gex_cache::sha256::hex(&h.finish()));
        let pg = gex_xlate::summary::profile_guards(&prog, &profile, &id, image.bias);
        eprintln!("profile {pf}: {} guarded call sites for {id}", pg.calls.len());
        guards.calls.extend(pg.calls);
    }
    if !guards.calls.is_empty() {
        eprintln!("guards: {} indirect call sites", guards.calls.len());
    }
    let sums = gex_xlate::summary::analyze_with_guards(&prog, &guards);
    let t_sum = t1.elapsed();
    let st = &prog.stats;
    let pct = |a: u64, b: u64| if b == 0 { 100.0 } else { 100.0 * a as f64 / b as f64 };
    eprintln!(
        "functions {}  blocks {}  rounds {}  ({:.2}s recovery, {:.2}s summaries)",
        st.functions,
        st.blocks,
        st.rounds,
        t_rec.as_secs_f64(),
        t_sum.as_secs_f64()
    );
    eprintln!(
        "symbols {}/{} found ({:.2}%)  fdes {}/{}  symbol bytes covered {:.2}% of {}",
        st.symbols_found,
        st.symbols,
        pct(st.symbols_found as u64, st.symbols as u64),
        st.fdes_found,
        st.fdes,
        pct(st.symbol_bytes_covered, st.symbol_bytes),
        st.symbol_bytes
    );
    let (cb, cc) = gex_xlate::func::code_coverage(&prog, &image.code);
    eprintln!(
        "fde bytes covered {:.2}% of {}  code bytes covered {:.2}% of {}",
        pct(st.fde_bytes_covered, st.fde_bytes),
        st.fde_bytes,
        pct(cc, cb),
        cb
    );
    let torn: usize = prog
        .functions
        .values()
        .map(|f| gex_xlate::summary::frame_torn_down(f, &sums).len())
        .sum();
    eprintln!(
        "switches {} complete, {} partial; {} unresolved indirect jumps ({} with the frame torn down); {} noreturn calls; {} interior entries",
        st.switches_complete, st.switches_partial, st.unresolved, torn, st.noreturn_calls, st.interior_entries
    );
    eprintln!(
        "summaries: {} SCCs, {} recursive, max {} iterations, {} not converged; {} functions with unknown callees",
        sums.stats.sccs,
        sums.stats.recursive_sccs,
        sums.stats.max_iterations,
        sums.stats.unconverged,
        sums.stats.unknown
    );
    if let Some(t) = &truth {
        let text = std::fs::read_to_string(t).with_context(|| format!("reading {t}"))?;
        let starts: Vec<u64> = text
            .split_whitespace()
            .filter_map(|w| u64::from_str_radix(w.trim_start_matches("0x"), 16).ok())
            .collect();
        let (n, f, inside, bytes, cov) = gex_xlate::func::truth_coverage(&prog, &image.code, &starts);
        eprintln!(
            "truth {t}: {f}/{n} starts are functions ({:.2}%), {inside} inside a block ({:.2}%); bytes covered {:.2}% of {bytes}",
            pct(f as u64, n as u64),
            pct(inside as u64, n as u64),
            pct(cov, bytes)
        );
    }
    if !sums.stats.unconverged_members.is_empty() {
        let m: Vec<String> = sums.stats.unconverged_members.iter().take(20).map(|a| format!("{a:#x}")).collect();
        eprintln!("  not converged: {}", m.join(" "));
    }
    let missing: Vec<(&u64, &gex_xlate::func::StartInfo)> = prog
        .starts
        .iter()
        .filter(|(a, s)| s.has(gex_xlate::func::Source::Symbol) && !prog.functions.contains_key(a))
        .collect();
    for (a, s) in missing.iter().take(20) {
        eprintln!("  missing symbol {a:#x} {}", s.name.as_deref().unwrap_or("?"));
    }
    if show_uncovered {
        let view = gex_xlate::CodeView::new(&image);
        for (lo, hi, sym) in gex_xlate::func::uncovered(&prog) {
            let name = prog.starts[&sym].name.as_deref().unwrap_or("?");
            println!("uncovered {lo:#x}..{hi:#x} in {name} ({sym:#x})");
            let mut pc = lo;
            while pc < hi {
                let mut buf = [0u8; 16];
                let n = view.fetch(pc, &mut buf);
                if n == 0 {
                    break;
                }
                let (text, len) = gex_xlate::arch::disasm(&buf[..n], pc);
                println!("    {pc:#x}: {text}");
                pc += len as u64;
            }
        }
    }
    if show_unresolved {
        for f in prog.functions.values() {
            let torn = gex_xlate::summary::frame_torn_down(f, &sums);
            for b in &f.blocks {
                if matches!(b.term, gex_ir::func::FTerm::Unresolved { .. }) {
                    let shape = if torn.contains(&b.term_pc) { "torn-down" } else { "-" };
                    println!("unresolved {:#x} in {:#x} {shape}", b.term_pc, f.entry);
                }
            }
        }
    }
    if let Some(a) = at.filter(|a| !prog.functions.contains_key(a)) {
        let src = prog.starts.get(&a).map(|s| s.sources());
        let owners: Vec<String> = prog
            .functions
            .values()
            .filter(|f| f.blocks.iter().any(|b| !b.synthetic && b.insts.contains(&a)))
            .map(|f| format!("{:#x}", f.entry))
            .collect();
        println!("{a:#x} is not a function; seeds {src:?}; inside {}", owners.join(" "));
    }
    let mut bad = 0usize;
    let mut vrep = gex_xlate::validate::Report::default();
    for (&a, f) in &prog.functions {
        if at.is_some_and(|x| x != a) {
            continue;
        }
        let summ = sums.get(a);
        if list || at.is_some() {
            let srcs: Vec<String> = prog.starts[&a].sources().iter().map(|s| format!("{s:?}")).collect();
            println!(
                "{a:#x} {} blocks={} calls={} entries={} [{}] {}",
                f.name.as_deref().unwrap_or("-"),
                f.blocks.len(),
                f.call_sites().len(),
                f.entries.len(),
                srcs.join(","),
                gex_xlate::summary::describe(&summ)
            );
        }
        let ssa_f = if ssa || check {
            Some(sums.to_ssa(f))
        } else {
            None
        };
        if ir {
            print!("{}", gex_ir::text::print(f));
        }
        if ssa {
            print!("{}", gex_ir::text::print(ssa_f.as_ref().unwrap()));
        }
        if check {
            for g in [f, ssa_f.as_ref().unwrap()] {
                let r = g.verify().and_then(|_| {
                    let t = gex_ir::text::print(g);
                    let back = gex_ir::text::parse(&t)?;
                    if &back != g {
                        return Err("text round trip changed the function".into());
                    }
                    Ok(())
                });
                if let Err(e) = r {
                    bad += 1;
                    eprintln!("{a:#x} {:?} form: {e}", g.form);
                }
            }
            if runs > 0 {
                gex_xlate::validate::check_function(a, f, &sums, runs, 2000, &mut vrep);
            }
        }
    }
    if check && runs > 0 {
        eprintln!(
            "run: {} functions, {} runs ({} to an exit the interpreter models, {} returns checked against summaries), {} failures",
            vrep.functions,
            vrep.runs,
            vrep.runs_complete,
            vrep.returns_checked,
            vrep.failures.len()
        );
        for f in vrep.failures.iter().take(40) {
            eprintln!("  {f}");
        }
        bad += vrep.failures.len();
    }
    if check {
        eprintln!("check: {bad} failures");
        if bad > 0 {
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `gexc profile merge|show`: work with profiles `gexc run --record` wrote.
fn cmd_profile(args: &[String]) -> Result<()> {
    use gex_xlate::profile::Profile;
    gex_native::profile::use_kernel_syscall_names();
    let read = |p: &str| -> Result<Profile> {
        let text = std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?;
        Profile::from_json(&text).map_err(|e| anyhow::anyhow!("{p}: {e}"))
    };
    match args.first().map(String::as_str) {
        Some("merge") => {
            let mut out: Option<String> = None;
            let mut split: Option<String> = None;
            let mut inputs = Vec::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "-o" => {
                        i += 1;
                        out = Some(args.get(i).context("-o needs a path")?.clone());
                    }
                    "--split" => {
                        i += 1;
                        split = Some(args.get(i).context("--split needs a directory")?.clone());
                    }
                    other => inputs.push(other.to_string()),
                }
                i += 1;
            }
            if inputs.is_empty() {
                bail!("profile merge: no input profiles");
            }
            if out.is_none() && split.is_none() {
                bail!("profile merge: give -o OUT or --split DIR");
            }
            let mut acc = Profile::default();
            for p in &inputs {
                acc.merge(&read(p)?).map_err(|e| anyhow::anyhow!("{p}: {e}"))?;
            }
            if let Some(o) = &out {
                std::fs::write(o, acc.to_json()).with_context(|| format!("writing {o}"))?;
                eprintln!("{o}: {} objects from {} profiles", acc.objects.len(), inputs.len());
            }
            if let Some(dir) = &split {
                // One file per object, named by its hash, merged into whatever
                // the directory already holds: a store every program adds to.
                std::fs::create_dir_all(dir).with_context(|| format!("creating {dir}"))?;
                for id in acc.objects.keys() {
                    if id == gex_xlate::profile::ANON {
                        continue;
                    }
                    let file = format!("{dir}/{}.json", id.replace(':', "-"));
                    let mut one = acc.only(id);
                    if std::path::Path::new(&file).exists() {
                        let mut old = read(&file)?;
                        old.merge(&one).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
                        one = old;
                    }
                    std::fs::write(&file, one.to_json()).with_context(|| format!("writing {file}"))?;
                }
                eprintln!("{dir}: {} objects", acc.objects.len());
            }
            Ok(())
        }
        Some("show") => {
            let mut top = 15;
            let mut input = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--top" => {
                        i += 1;
                        top = args.get(i).context("--top needs a number")?.parse()?;
                    }
                    other => input = Some(other.to_string()),
                }
                i += 1;
            }
            let p = read(&input.context("profile show: which profile?")?)?;
            print!("{}", p.summary(top));
            Ok(())
        }
        _ => bail!("usage: gexc profile merge (-o OUT | --split DIR) IN... | gexc profile show [--top N] IN"),
    }
}

#[cfg(test)]
mod cgen_profile_tests {
    use super::parse_run_args;

    #[test]
    fn cgen_with_record_fails_loudly() {
        let args: Vec<String> = ["--cgen", "--record", "/tmp/x", "prog"].iter().map(|s| s.to_string()).collect();
        let err = parse_run_args(&args).err().expect("--cgen with --record must be refused");
        assert!(format!("{err}").contains("--record"), "{err}");
        let ok: Vec<String> = ["--cgen", "prog"].iter().map(|s| s.to_string()).collect();
        if std::env::var_os("GEX_PROFILE").is_none() {
            assert!(parse_run_args(&ok).is_ok());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library's `memcpy`, placed at a base: the intrinsic goes at the
    /// address it is loaded at, not that plus the base again (which is
    /// outside its text, so it was silently dropped).
    #[test]
    fn a_placed_librarys_memcpy_is_found_where_it_is() {
        let base = gex_abi::MMAP_BOTTOM;
        let at = base + 0x100;
        let image = gex_ld::Image {
            soname: None,
            needed: Vec::new(),
            runpath: Vec::new(),
            bias: base,
            entry: base,
            code: vec![(base, base + 0x1000)],
            segments: Vec::new(),
            phdr_vaddr: base,
            phentsize: 56,
            phnum: 0,
            brk: base + 0x1000,
            text_range: (base, base + 0x1000),
            tls: None,
            class32: false,
            symbols: vec![gex_ld::Symbol {
                name: "memcpy".into(),
                value: at,
                size: 16,
                ifunc: false,
            }],
            named_relocs: Vec::new(),
            relocs: Vec::new(),
            init_array: Vec::new(),
            dynsyms: Vec::new(),
            irelative: Vec::new(),
            pie: true,
            interp: None,
        };
        let objects = vec![gex_ld::closure::Object {
            path: "/lib/libc.so".into(),
            name: "libc.so".into(),
            image,
            span: 0x1000,
            depth: 1,
        }];
        let got = closure_intrinsics(&objects);
        assert_eq!(got.get(&at), Some(&gex_xlate::Intrinsic::MemCpy));
        assert_eq!(got.len(), 1);

        // An i386 closure gets none: the intrinsic block is the 64-bit
        // calling convention, and cdecl passes the arguments on the stack.
        let mut objects = objects;
        objects[0].image.class32 = true;
        assert!(closure_intrinsics(&objects).is_empty());
    }
}

/// `gexc hotset`: see the usage text.
fn cmd_hotset(args: &[String]) -> Result<()> {
    let (mut path, mut samples, mut out, mut pe_base) = (None, None, None, None);
    let mut budget = 4000usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--samples" => {
                i += 1;
                samples = Some(args.get(i).context("--samples needs a file")?.clone());
            }
            "-o" => {
                i += 1;
                out = Some(args.get(i).context("-o needs a path")?.clone());
            }
            "--budget" => {
                i += 1;
                budget = args.get(i).context("--budget needs a number")?.parse()?;
            }
            "--pe-base" => {
                i += 1;
                let v = args.get(i).context("--pe-base needs an address")?;
                pe_base = Some(u64::from_str_radix(v.trim_start_matches("0x"), 16)?);
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected an image")?;
    let samples = samples.context("--samples FILE is required")?;
    let out = out.context("-o OUT is required")?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    let prog = if gex_ld::pe::is_pe(&bytes) {
        let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        let base = pe_base.unwrap_or(pe.preferred_base);
        let (image, extra) = gex_xlate::func::pe_input(&pe, base).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        gex_xlate::func::recover_pe(&image, &extra, &Default::default())
    } else {
        let image = gex_ld::elf::load(&bytes, gex_abi::PIE_BASE)?;
        gex_xlate::set_compat(image.class32);
        gex_xlate::func::recover(&image, Some(&bytes), &Default::default())
    };
    // Every real block of every function, by start: the functions holding it.
    let mut blocks: Vec<(u64, u64, u64)> = Vec::new();
    for (&entry, f) in &prog.functions {
        for b in f.blocks.iter().filter(|b| !b.synthetic) {
            blocks.push((b.start, b.end, entry));
        }
    }
    blocks.sort_unstable();
    let mut weight: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let text = std::fs::read_to_string(&samples).with_context(|| format!("reading {samples}"))?;
    let (mut total, mut hit) = (0u64, 0u64);
    for line in text.lines() {
        let mut w = line.split_whitespace();
        let (Some(pc), Some(n)) = (w.next(), w.next()) else { continue };
        let (Ok(pc), Ok(n)) = (u64::from_str_radix(pc, 16), n.parse::<u64>()) else { continue };
        total += n;
        // Blocks starting at or before pc whose range holds it.
        let k = blocks.partition_point(|b| b.0 <= pc);
        let mut any = false;
        for b in blocks[..k].iter().rev().take(64) {
            if pc < b.1 {
                *weight.entry(b.2).or_default() += n;
                any = true;
            }
        }
        if any {
            hit += n;
        }
    }
    let mut hot: Vec<(u64, u64)> = weight.into_iter().collect();
    hot.sort_by_key(|(a, n)| (std::cmp::Reverse(*n), *a));
    // The hot functions, hottest first, each followed by its direct callees
    // (a call to a function outside the set leaves the function tier).
    let mut chosen: Vec<u64> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut queue: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    for (a, _) in &hot {
        if chosen.len() >= budget {
            break;
        }
        queue.push_back(*a);
        while let Some(f) = queue.pop_front() {
            if chosen.len() >= budget || !seen.insert(f) {
                continue;
            }
            let Some(func) = prog.functions.get(&f) else { continue };
            chosen.push(f);
            for b in &func.blocks {
                if let gex_ir::func::FTerm::Call { callee: gex_ir::func::Callee::Direct(t), .. }
                | gex_ir::func::FTerm::TailCall { callee: gex_ir::func::Callee::Direct(t) } = &b.term
                {
                    if prog.functions.contains_key(t) && !seen.contains(t) {
                        queue.push_back(*t);
                    }
                }
            }
        }
    }
    let wmap: std::collections::HashMap<u64, u64> = hot.iter().copied().collect();
    let mut text = format!(
        "# gexc hotset {path}: {} of {} recovered functions; {hit} of {total} samples in them\n",
        chosen.len(),
        prog.functions.len()
    );
    for f in &chosen {
        let name = prog.functions[f].name.as_deref().unwrap_or("");
        text.push_str(&format!("{f:x} # {} {name}\n", wmap.get(f).copied().unwrap_or(0)));
    }
    std::fs::write(&out, text).with_context(|| format!("writing {out}"))?;
    eprintln!(
        "{out}: {} functions ({} sampled); {hit} of {total} samples land in recovered functions",
        chosen.len(),
        hot.len()
    );
    Ok(())
}

/// The DLLs `gexc cgen-prefix` compiles by default: what Noita and tests/nt
/// map, measured (`GEX_NT_TRACE_IMAGES=1`).
const CGEN_CORE: &str = include_str!("../../../tools/nt-cgen-core.txt");

/// `gexc cgen-prefix`: see the usage text.
fn cmd_cgen_prefix(args: &[String]) -> Result<()> {
    use gex_kernel::nt::section;
    let (mut prefix, mut set, mut jobs, mut layout_only) = (None, None, 4usize, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--set" => {
                i += 1;
                set = Some(std::fs::read_to_string(args.get(i).context("--set needs a file")?)?);
            }
            "--jobs" => {
                i += 1;
                jobs = args.get(i).context("--jobs needs a number")?.parse()?;
            }
            "--layout-only" => layout_only = true,
            other => prefix = Some(other.to_string()),
        }
        i += 1;
    }
    let prefix = match prefix {
        Some(p) => p,
        None => std::env::var("GEX_WINE_PREFIX")
            .unwrap_or_else(|_| format!("{}/.cache/gex/wine-11.15", std::env::var("HOME").unwrap_or_default())),
    };
    let prefix = std::fs::canonicalize(&prefix)
        .with_context(|| format!("no staged prefix at {prefix}"))?
        .to_string_lossy()
        .into_owned();
    let wine = std::fs::read_to_string(format!("{prefix}/.wine-version"))
        .with_context(|| format!("{prefix} is not a staged prefix (no .wine-version)"))?;
    let names: Vec<String> = set
        .as_deref()
        .unwrap_or(CGEN_CORE)
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_ascii_lowercase())
        .filter(|l| !l.is_empty())
        .collect();
    let t_all = std::time::Instant::now();
    let mut layout = Vec::new();
    let mut todo = Vec::new();
    for dir in ["drive_c/windows/syswow64", "drive_c/windows/system32"] {
        // Which preferred bases are a DLL's own, over the whole directory.
        let mut prefs: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for e in std::fs::read_dir(format!("{prefix}/{dir}"))? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")) {
                if let Ok(pe) = std::fs::read(&p).map_err(anyhow::Error::from).and_then(|b| gex_ld::pe::parse(&b).map_err(|e| anyhow::anyhow!("{e}"))) {
                    *prefs.entry(pe.preferred_base).or_default() += 1;
                }
            }
        }
        let mut images = Vec::new();
        for n in &names {
            let rel = format!("{dir}/{n}");
            let path = format!("{prefix}/{rel}");
            let Ok(bytes) = std::fs::read(&path) else {
                eprintln!("cgen-prefix: {rel} is not in the prefix; skipped");
                continue;
            };
            let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
            let mut h = gex_cache::sha256::Sha256::new();
            h.update(&bytes);
            let sha = gex_cache::sha256::hex(&h.finish());
            let own = prefs.get(&pe.preferred_base).copied().unwrap_or(0) == 1;
            images.push((rel, pe.preferred_base, pe.image_size as u64, sha, own));
        }
        let planned = section::plan_layout(&images);
        for (rel, b, _, _) in &planned {
            todo.push((format!("{prefix}/{rel}"), *b));
        }
        layout.extend(planned);
    }
    let text = section::render_layout(&layout, &wine);
    let tmp = format!("{prefix}/{}.tmp", section::LAYOUT_FILE);
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, format!("{prefix}/{}", section::LAYOUT_FILE))?;
    eprintln!("cgen-prefix: {} images laid out in {prefix}/{}", layout.len(), section::LAYOUT_FILE);
    if layout_only {
        return Ok(());
    }
    let mut wasm_total = 0u64;
    let _lock = gex_native::nt_cgen::build_lock()?;
    for (path, base) in &todo {
        let bytes = std::fs::read(path)?;
        let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        let t0 = std::time::Instant::now();
        let out = gex_native::nt_cgen::prepare_pe(&pe, *base, jobs, false)
            .with_context(|| format!("building the function tier for {path} at {base:#x}"))?;
        let n = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        wasm_total += n;
        let name = path.rsplit('/').next().unwrap_or(path);
        let dir = path.rsplit('/').nth(1).unwrap_or("");
        eprintln!("cgen-prefix: {dir}/{name} at {base:#x}: {} KiB wasm, {:.1}s", n >> 10, t0.elapsed().as_secs_f64());
    }
    eprintln!(
        "cgen-prefix: {} units, {} MiB wasm, {:.1}s",
        todo.len(),
        wasm_total >> 20,
        t_all.elapsed().as_secs_f64()
    );
    Ok(())
}

/// `gexc cgen-image`: see the usage text.
/// `gexc cgen-c <pe> [--pe-base HEX] [--recovery DIR] --at ADDR`: print the C the function
/// tier's unit holds for one function, from the image's cached recovery
/// (`gexc cgen-image` makes it).
fn cmd_cgen_c(args: &[String]) -> Result<()> {
    let (mut path, mut pe_base, mut at, mut rec) = (None, None, None, None);
    let mut i = 0;
    while i < args.len() {
        let hex = |v: Option<&String>| -> Result<u64> { Ok(u64::from_str_radix(v.context("needs an address")?.trim_start_matches("0x"), 16)?) };
        match args[i].as_str() {
            "--pe-base" => {
                i += 1;
                pe_base = Some(hex(args.get(i))?);
            }
            "--at" => {
                i += 1;
                at = Some(hex(args.get(i))?);
            }
            // Print the header the C includes (gex_abi.h) instead.
            "--header" => {
                print!("{}", gex_cgen::header::header());
                return Ok(());
            }
            // A recovery directory to use rather than this build's key's.
            "--recovery" => {
                i += 1;
                rec = Some(std::path::PathBuf::from(args.get(i).context("--recovery needs a directory")?));
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected a PE image")?;
    let at = at.context("--at is required")?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let base = pe_base.unwrap_or(pe.preferred_base);
    let (image, _) = gex_xlate::func::pe_input(&pe, base).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let dir = match rec {
        Some(d) => d,
        None => gex_native::nt_cgen::cache_root()?.join("recovery").join(gex_native::nt_cgen::recovery_key(&image)),
    };
    let read = |a: u64| image.segments.iter().find_map(|s| a.checked_sub(s.vaddr).and_then(|o| s.bytes.get(o as usize)).copied());
    print!("{}", gex_cgen::image::function_c(&dir, at, &read)?);
    Ok(())
}

fn cmd_cgen_image(args: &[String]) -> Result<()> {
    let (mut path, mut pe_base) = (None, None);
    let mut jobs = 4usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pe-base" => {
                i += 1;
                let v = args.get(i).context("--pe-base needs an address")?;
                pe_base = Some(u64::from_str_radix(v.trim_start_matches("0x"), 16)?);
            }
            "--jobs" => {
                i += 1;
                jobs = args.get(i).context("--jobs needs a number")?.parse()?;
            }
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected a PE image")?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {path}"))?;
    let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
    let base = pe_base.unwrap_or(pe.preferred_base);
    // One build at a time per cache, across processes: a recovery's peak is
    // the machine's, and runs queue builds for the next run in the
    // background.
    let _lock = gex_native::nt_cgen::build_lock()?;
    let t0 = std::time::Instant::now();
    let out = gex_native::nt_cgen::prepare_pe(&pe, base, jobs, true)?;
    eprintln!("{path} at {base:#x}: {} ({:.1}s)", out.display(), t0.elapsed().as_secs_f64());
    Ok(())
}
