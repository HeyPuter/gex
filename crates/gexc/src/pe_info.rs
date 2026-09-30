//! `gexc pe-info`: load a Windows program's PE closure and print what the
//! loader made of it (see `gex_ld::pe`).

use anyhow::{bail, Context, Result};
use gex_ld::pe::{self, PeOptions, PeProgram, PlaceholderReason, Reason, Target};
use std::fmt::Write as _;

pub const USAGE: &str = "\
  gexc pe-info <exe> [--dll-dir DIR]...
                                   load a PE32/PE32+ program and the DLLs it
                                   reaches (its own directory first, then each
                                   --dll-dir), place and relocate them, bind
                                   imports, and print the layout, relocation
                                   counts, imports (resolved or placeholder),
                                   exports, TLS callbacks and delay imports.
                                   --extra-dll PATH  also load this DLL
                                   --native-dll NAME load this system DLL
                                                     from disk after all
                                   --no-delay-dlls   do not load delay-
                                                     imported DLLs
                                   --no-pack         keep a relocatable image
                                                     at its preferred base
                                                     when it fits (the default
                                                     packs everything low, as
                                                     the NT layer does)
                                   --base NAME=ADDR  pin a module's base
                                   --brief           counts, not symbol lists
                                   (`gexc closure` on a PE image does this too)";

pub fn cmd_pe_info(args: &[String]) -> Result<()> {
    let mut path = None;
    let mut opts = PeOptions::default();
    let mut brief = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let mut value = |what: &str| -> Result<String> {
            i += 1;
            args.get(i).cloned().with_context(|| format!("{what} needs a value"))
        };
        match arg {
            "--dll-dir" => opts.search_dirs.push(value("--dll-dir")?),
            "--extra-dll" => opts.extra_dlls.push(value("--extra-dll")?),
            "--native-dll" => {
                opts.native_dlls.insert(pe::normalize_dll(&value("--native-dll")?));
            }
            "--no-delay-dlls" => opts.skip_delay_dlls = true,
            "--no-pack" => opts.layout.pack = false,
            "--brief" => brief = true,
            "--base" => {
                let v = value("--base")?;
                let (n, a) = v.split_once('=').context("--base wants NAME=ADDR")?;
                let a = a.trim_start_matches("0x");
                let addr = u64::from_str_radix(a, 16).with_context(|| format!("--base address {a:?}"))?;
                opts.layout.pins.insert(pe::normalize_dll(n), addr);
            }
            a if a.starts_with('-') => bail!("pe-info: unknown option {a}"),
            other => path = Some(other.to_string()),
        }
        i += 1;
    }
    let path = path.context("expected an input PE executable")?;
    let prog = pe::load_closure(&path, &pe::HostFiles, &opts)?;
    print!("{}", report(&prog, brief));
    Ok(())
}

fn perms(r: bool, w: bool, x: bool) -> String {
    format!("{}{}{}", if r { 'r' } else { '-' }, if w { 'w' } else { '-' }, if x { 'x' } else { '-' })
}

fn machine(m: u16) -> &'static str {
    match m {
        pe::MACHINE_I386 => "i386",
        pe::MACHINE_AMD64 => "AMD64",
        pe::MACHINE_ARM64 => "ARM64",
        _ => "?",
    }
}

fn target(prog: &PeProgram, t: &Target) -> String {
    match t {
        Target::Image { module, va, via } => {
            let mut s = format!("{:#x} in {}", va, prog.modules[*module].name);
            if !via.is_empty() {
                let _ = write!(s, " (via {})", via.join(" -> "));
            }
            s
        }
        Target::Placeholder(i) => {
            let p = &prog.placeholders.entries[*i as usize];
            let why = match p.reason {
                PlaceholderReason::System => "system",
                PlaceholderReason::MissingDll => "DLL NOT FOUND",
                PlaceholderReason::MissingExport => "EXPORT NOT FOUND",
            };
            let mut s = format!("placeholder #{} {:#x} [{}] {}", p.index, p.addr, why, p.label());
            if !p.via.is_empty() {
                let _ = write!(s, " (via {})", p.via.join(" -> "));
            }
            s
        }
    }
}

/// The text `pe-info` prints.
pub fn report(prog: &PeProgram, brief: bool) -> String {
    let mut o = String::new();
    let exe = &prog.modules[0];
    let _ = writeln!(
        o,
        "{}: {} {}, entry {:#x}, {} module(s), {} placeholder import(s)",
        exe.path,
        if exe.pe.is64 { "PE32+" } else { "PE32" },
        machine(exe.pe.machine),
        prog.entry(),
        prog.modules.len(),
        prog.placeholders.entries.len()
    );
    let _ = writeln!(o, "\nlayout:");
    for (i, m) in prog.modules.iter().enumerate() {
        let why = match &m.reason {
            Reason::Executable => "executable".to_string(),
            Reason::Import { by } => format!("imported by {by}"),
            Reason::Forwarder { by } => format!("forwarder target of {by}"),
            Reason::Delay { by } => format!("delay-loaded by {by}"),
        };
        let _ = writeln!(
            o,
            "  {i:2} {:<20} {:#010x}-{:#010x}  preferred {:#x}{}  {}",
            m.name,
            m.base,
            m.end(),
            m.pe.preferred_base,
            if m.rebased { " REBASED" } else { "" },
            why
        );
    }
    for m in &prog.modules {
        let pe = &m.pe;
        let _ = writeln!(o, "\n== {} ({})", m.name, m.path);
        let _ = writeln!(
            o,
            "  {} {} {}, subsystem {}, base {:#x}, size {:#x}, entry {}",
            if pe.is64 { "PE32+" } else { "PE32" },
            machine(pe.machine),
            if pe.is_dll() { "DLL" } else { "EXE" },
            pe.subsystem,
            m.base,
            pe.image_size,
            m.entry().map(|e| format!("{e:#x}")).unwrap_or_else(|| "none".into())
        );
        let _ = writeln!(o, "  sections:");
        for s in &pe.sections {
            let _ = writeln!(
                o,
                "    {:<8} {:#010x} vsize {:#8x} raw {:#8x} {}",
                s.name,
                m.base + s.rva as u64,
                s.vsize,
                s.raw_size,
                perms(s.readable(), s.writable(), s.executable())
            );
        }
        let c = pe.reloc_counts;
        let _ = writeln!(
            o,
            "  relocations: {} (HIGHLOW {}, DIR64 {}; {} ABSOLUTE padding, {} blocks), {}",
            c.highlow + c.dir64,
            c.highlow,
            c.dir64,
            c.absolute,
            c.blocks,
            match (m.rebased, c.highlow + c.dir64) {
                (false, _) => "not needed at the preferred base",
                (true, 0) => "moved, nothing to fix up",
                (true, _) => "applied",
            }
        );
        let _ = writeln!(o, "  imports: {} slot(s)", m.imports.len());
        if !brief {
            for b in &m.imports {
                let _ = writeln!(o, "    {:#010x} {}!{} -> {}", b.slot, b.dll, b.import.label(), target(prog, &b.target));
            }
        }
        for b in &pe.bound_imports {
            let _ = writeln!(o, "  bound import: {} (timestamp {:#x}), rebound from the name table", b.dll, b.timestamp);
        }
        if !m.delay_imports.is_empty() {
            let _ = writeln!(o, "  delay imports: {} slot(s)", m.delay_imports.len());
            for d in &m.delay_imports {
                let _ = writeln!(
                    o,
                    "    {:#010x} {}!{} thunk {:#x} -> {}",
                    d.slot,
                    d.dll,
                    d.import.label(),
                    d.thunk,
                    target(prog, &d.target)
                );
            }
        }
        let ex = &pe.exports;
        let live: Vec<_> = ex.entries.iter().filter(|e| e.rva != 0).collect();
        if !live.is_empty() {
            let _ = writeln!(o, "  exports: {} (\"{}\", ordinal base {})", live.len(), ex.dll_name, ex.ordinal_base);
            if !brief {
                for e in live {
                    let names = if e.names.is_empty() { "(by ordinal only)".to_string() } else { e.names.join(", ") };
                    match &e.forwarder {
                        Some(f) => {
                            let _ = writeln!(o, "    #{:<5} {} -> forwarder {}", e.ordinal, names, f);
                        }
                        None => {
                            let _ = writeln!(o, "    #{:<5} {} {:#x}", e.ordinal, names, m.base + e.rva as u64);
                        }
                    }
                }
            }
        }
        if let Some(t) = &pe.tls {
            let rebase = |va: u64| if va == 0 { 0 } else { va - pe.preferred_base + m.base };
            let cbs: Vec<String> = m.tls_callbacks.iter().map(|c| format!("{c:#x}")).collect();
            let _ = writeln!(
                o,
                "  TLS: index {:#x}, template {:#x}-{:#x}, zero fill {:#x}, callbacks [{}]",
                rebase(t.index_va),
                rebase(t.start_raw_va),
                rebase(t.end_raw_va),
                t.zero_fill,
                cbs.join(", ")
            );
        }
        if !pe.resources.is_empty() {
            let _ = writeln!(o, "  resources: {}", pe.resources.len());
            if !brief {
                for r in &pe.resources {
                    let p: Vec<String> = r.path.iter().map(|x| x.to_string()).collect();
                    let _ = writeln!(o, "    {} at {:#x}, {} bytes", p.join("/"), m.base + r.data_rva as u64, r.size);
                }
            }
        }
        if !pe.pdata.is_empty() {
            let chained = pe.pdata.iter().filter(|f| f.unwind_flags.is_some_and(|x| x & 4 != 0)).count();
            let handlers = pe.pdata.iter().filter(|f| f.unwind_flags.is_some_and(|x| x & 3 != 0)).count();
            let _ = writeln!(
                o,
                "  .pdata: {} function(s), {} with a handler, {} chained",
                pe.pdata.len(),
                handlers,
                chained
            );
        }
    }
    if !prog.placeholders.entries.is_empty() {
        let _ = writeln!(
            o,
            "\nplaceholders ({:#x}-{:#x}, {} bytes each):",
            pe::PLACEHOLDER_BASE,
            pe::PLACEHOLDER_END,
            pe::PLACEHOLDER_STRIDE
        );
        for p in &prog.placeholders.entries {
            let slots = p.slots.iter().filter(|s| !s.delay).count();
            let delay = p.slots.len() - slots;
            let _ = writeln!(
                o,
                "  #{:<4} {:#x} {:?} {} ({} slot(s){})",
                p.index,
                p.addr,
                p.reason,
                p.label(),
                slots,
                if delay > 0 { format!(", {delay} delay") } else { String::new() }
            );
        }
    }
    if !prog.missing.is_empty() {
        let _ = writeln!(o, "\nmissing DLLs:");
        for (dll, by) in &prog.missing {
            let _ = writeln!(o, "  {dll} (named by {by})");
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> String {
        format!("{}/../gex-ld/tests/pe/bin/{rel}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn the_report_shows_layout_imports_exports_tls_and_delay_imports() {
        let prog = pe::load_closure(&fixture("x64/uses.exe"), &pe::HostFiles, &PeOptions::default()).unwrap();
        let r = report(&prog, false);
        for want in [
            "PE32+ AMD64",
            "preferred 0x140000000 REBASED  executable",
            "delay-loaded by uses.exe",
            "relocations: 45 (HIGHLOW 0, DIR64 45",
            "liba.dll!a_fwd_b -> 0x",
            "(via liba.dll!a_fwd_b)",
            "liba.dll!#7 -> 0x",
            "[system] kernel32.dll!HeapAlloc (via liba.dll!a_heap)",
            "#7     (by ordinal only) 0x",
            "#9     a_fwd_ord -> forwarder libb.#3",
            "delayed.dll!d_mul thunk 0x",
            "TLS: index 0x",
            "10/\"GREETING\"/1033",
            ".pdata: ",
            "placeholders (0x300000-0x400000, 16 bytes each):",
        ] {
            assert!(r.contains(want), "no {want:?} in:\n{r}");
        }
        assert!(!r.contains("missing DLLs"));
    }

    #[test]
    fn options_parse_and_errors_name_the_file() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        cmd_pe_info(&args(&[&fixture("x86/uses.exe"), "--brief", "--no-pack", "--base", "libb=0x2000000"])).unwrap();
        let e = cmd_pe_info(&args(&[&fixture("x64/highbase-fixed.exe")])).unwrap_err();
        assert!(format!("{e:#}").contains("highbase-fixed.exe: PE placement"), "{e:#}");
        assert!(cmd_pe_info(&args(&["--bogus"])).is_err());
    }
}
