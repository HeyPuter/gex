//! Export providers (docs/merge-plan.md §9.9.1): native wasm in place of a
//! guest library's exports.
//!
//! A provider source (`providers/*.c`) names its libraries on its first line
//! (`/* PROVIDES <dll>[,<dll>...] <i386|x86_64>`; an export a library lacks
//! is skipped) and binds F_body functions to exports
//! with `GEX_PROVIDE(fn, "export")` (providers/provider.h). It is compiled
//! with the units' toolchain into a position-independent module of its own;
//! when the library maps, the host binds each function at its exports' guest
//! addresses in the function table, after the library's unit, whose bodies
//! for those addresses are stubs that re-enter the table ([`stubs`]), so the
//! library's own calls reach the provider too.
//!
//! Words after the architecture on the first line:
//! * `stateful`: the provider may have static data and a C stack; the host
//!   gives it both as it gives an ejected project's stateful providers
//!   (shims/native libprov: data in guest pages from the NT allocator, one
//!   stack per thread), and it may `#include` the files in [`INCLUDES`];
//! * `nowine`: not for Wine's own build of the library (one whose DOS stub
//!   says "Wine builtin DLL"), whose internals reach the same state by
//!   other paths ([`for_image`]);
//! * `wineonly`: only for Wine's own build (the counterpart of a `nowine`
//!   provider, written against Wine's internals).
//! * `buildid=<hex>[,<hex>...]` (ELF): only for those builds of the
//!   library, for a provider that depends on a layout outside the library's
//!   versioned ABI (docs/elf-providers.md §2).
//!
//! An ELF provider (`/* PROVIDES libc.so.6 x86_64`) names exports by
//! versioned symbol (`memcpy@@GLIBC_2.14`; a bare name is the default
//! version); the host binds an IFUNC's resolver to the source's
//! `gexp__ifunc` (`GEX_ELF_IFUNC`). `GEX_GROUP("a,b,...")` lines name
//! exports that bind together or not at all.
//!
//! `GEX_PROVIDERS=0` turns every provider off (A/B runs, lockstep);
//! `GEX_PROVIDER_SKIP=<export>[,<export>...]` leaves those exports to the
//! guest's own code (in the browser, which has no environment, the page's
//! `?provskip=` through [`set_skip`]).

use crate::{header, toolchain};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The exports [`set_skip`] named, in place of `GEX_PROVIDER_SKIP`.
static SKIP: RwLock<Option<String>> = RwLock::new(None);

/// Leave `list` (`export,export,...`) to the guest's own code, as
/// `GEX_PROVIDER_SKIP` does: for a host with no environment (the browser's
/// `?provskip=`). Before any unit key is made, or keys disagree.
pub fn set_skip(list: &str) {
    *SKIP.write().unwrap_or_else(|e| e.into_inner()) = Some(list.to_string());
}

fn skip_list() -> String {
    match &*SKIP.read().unwrap_or_else(|e| e.into_inner()) {
        Some(s) => s.clone(),
        None => std::env::var("GEX_PROVIDER_SKIP").unwrap_or_default(),
    }
}

const PRELUDE: &str = include_str!("../providers/provider.h");

/// Every provider source gex carries.
const SOURCES: &[(&str, &str)] = &[
    ("msvcr120_concrt", include_str!("../providers/msvcr120_concrt.c")),
    ("crt_mem", include_str!("../providers/crt_mem.c")),
    ("crt_math", include_str!("../providers/crt_math.c")),
    ("wine_libm", include_str!("../providers/wine_libm.c")),
    ("debug_string", include_str!("../providers/debug_string.c")),
    ("crt_heap", include_str!("../providers/crt_heap.c")),
    ("crt_heap_wine", include_str!("../providers/crt_heap_wine.c")),
    ("nt_qpc", include_str!("../providers/nt_qpc.c")),
    ("nt_unixcall", include_str!("../providers/nt_unixcall.c")),
    ("msvcp120_thrd", include_str!("../providers/msvcp120_thrd.c")),
    // Noita's FMOD, silent until FMOD's own wasm build binds there instead.
    ("fmodstudio_silent", include_str!("../providers/fmodstudio_silent.c")),
    ("fmod_silent", include_str!("../providers/fmod_silent.c")),
    ("glibc_mem", include_str!("../providers/glibc_mem.c")),
    ("glibc_printf", include_str!("../providers/glibc_printf.c")),
    ("glibc_math", include_str!("../providers/glibc_math.c")),
    // The same functions for aarch64's glibc: the x86-64 bodies under
    // AAPCS64 (`aapcs64.inc` makes the `gp64_` helpers mean its registers).
    (
        "glibc_mem_a64",
        concat!(
            "/* PROVIDES libc.so.6 aarch64 */\n",
            include_str!("../providers/aapcs64.inc"),
            include_str!("../providers/glibc_mem.c"),
            include_str!("../providers/glibc_mem_a64.inc")
        ),
    ),
    (
        "glibc_math_a64",
        concat!("/* PROVIDES libm.so.6 aarch64 */\n", include_str!("../providers/aapcs64.inc"), include_str!("../providers/glibc_math.c")),
    ),
];

/// Files a stateful provider may `#include` (written beside it when built).
const INCLUDES: &[(&str, &str)] = &[("dlmalloc.inc", include_str!("../providers/dlmalloc.inc"))];

/// The guest architecture a provider's calling convention is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    I386,
    X86_64,
    Aarch64,
}

/// The provider architecture for an image of this build's guest: i386 or
/// x86-64 on an x86 build, aarch64's 64-bit images on an aarch64 build
/// (its 32-bit ones are wasm32 programs, which have no providers).
pub fn guest_arch(class32: bool) -> Option<Arch> {
    match (gex_abi::X86, class32) {
        (true, true) => Some(Arch::I386),
        (true, false) => Some(Arch::X86_64),
        (false, false) => Some(Arch::Aarch64),
        (false, true) => None,
    }
}

/// One provider source, parsed.
#[derive(Clone, Debug)]
pub struct Provider {
    pub name: &'static str,
    text: &'static str,
    /// The libraries, lower case (`msvcr120.dll`).
    pub dlls: Vec<String>,
    pub arch: Arch,
    /// `arch` is i386.
    pub class32: bool,
    /// Has data and a stack (`stateful` on the first line).
    pub stateful: bool,
    /// Not for Wine's build of the library (`nowine` on the first line).
    pub nowine: bool,
    /// Only for Wine's build of the library (`wineonly`).
    pub wineonly: bool,
    /// Keeps the image's lifted body of every export it binds, as a twin
    /// it can fall back to (`lifted`; provider.h `GP_LIFTED`).
    pub lifted: bool,
    /// (C function, export), in source order.
    pub binds: Vec<(String, String)>,
    /// Exports that bind together or not at all (`GEX_GROUP`).
    pub groups: Vec<Vec<String>>,
    /// The library builds (ELF build-ids, lower-case hex) it is for; empty
    /// for any (`buildid=` on the first line).
    pub build_ids: Vec<String>,
}

/// Every provider gex carries for this build's guest (`gexc cgen-image
/// --providers` builds them all into a unit cache, for a host that cannot
/// build them at run time).
pub fn all() -> Vec<Provider> {
    let archs = [guest_arch(true), guest_arch(false)];
    SOURCES
        .iter()
        .map(|(n, t)| parse(n, t).expect("a provider source gex carries is well formed"))
        .filter(|p| archs.contains(&Some(p.arch)))
        .collect()
}

/// Whether providers are on (`GEX_PROVIDERS=0` turns them off).
pub fn enabled() -> bool {
    std::env::var("GEX_PROVIDERS").map_or(true, |v| v != "0")
}

fn parse(name: &'static str, text: &'static str) -> Result<Provider> {
    let first = text.lines().next().unwrap_or("");
    let w: Vec<&str> = first.split_whitespace().collect();
    if w.len() < 4 || w[0] != "/*" || w[1] != "PROVIDES" {
        bail!("provider {name}: the first line must be `/* PROVIDES <dll>[,<dll>...] <i386|x86_64|aarch64> [stateful] [nowine|wineonly] [lifted]`");
    }
    let stateful = w[4..].contains(&"stateful");
    let nowine = w[4..].contains(&"nowine");
    let wineonly = w[4..].contains(&"wineonly");
    let lifted = w[4..].contains(&"lifted");
    let build_ids = w[4..]
        .iter()
        .filter_map(|x| x.strip_prefix("buildid="))
        .flat_map(|x| x.split(','))
        .filter(|x| !x.is_empty())
        .map(|x| x.to_ascii_lowercase())
        .collect();
    let arch = match w[3] {
        "i386" => Arch::I386,
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        a => bail!("provider {name}: unknown architecture {a}"),
    };
    let class32 = arch == Arch::I386;
    let mut binds = Vec::new();
    let mut groups = Vec::new();
    for l in text.lines() {
        if let Some(rest) = l.trim().strip_prefix("GEX_GROUP(") {
            let list = rest.trim().trim_end_matches(')').trim().trim_matches('"');
            groups.push(list.split(',').map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect());
            continue;
        }
        let Some(rest) = l.trim().strip_prefix("GEX_PROVIDE(") else { continue };
        let (f, e) = rest
            .split_once(',')
            .ok_or_else(|| anyhow!("provider {name}: malformed {l:?}"))?;
        let e = e.trim().trim_end_matches(')').trim().trim_matches('"');
        // A later binding of an export replaces an earlier one (an
        // architecture's own body after a shared source's).
        binds.retain(|(_, x)| x != e);
        binds.push((f.trim().to_string(), e.to_string()));
    }
    if binds.is_empty() {
        bail!("provider {name} provides nothing");
    }
    let skip = skip_list();
    // A versioned export is skipped by its bare name too (`memcpy` skips
    // `memcpy@@GLIBC_2.14`).
    binds.retain(|(_, e)| !skip.split(',').any(|s| s == e || e.split('@').next() == Some(s)));
    let dlls = w[2].split(',').map(|d| d.to_ascii_lowercase()).collect();
    Ok(Provider { name, text, dlls, arch, class32, stateful, nowine, wineonly, lifted, binds, groups, build_ids })
}

/// The providers for library `dll` (its export directory's name, any case)
/// of the given class, for this build's guest ([`guest_arch`]), when
/// providers are on.
pub fn for_dll(dll: &str, class32: bool) -> Vec<Provider> {
    let Some(arch) = guest_arch(class32).filter(|_| enabled()) else {
        return Vec::new();
    };
    let dll = dll.to_ascii_lowercase();
    SOURCES
        .iter()
        .map(|(n, t)| parse(n, t).expect("a provider source gex carries is well formed"))
        .filter(|p| p.dlls.contains(&dll) && p.arch == arch)
        .collect()
}

/// The library name the NT trampoline page has for providers
/// (`/* PROVIDES [nt-trampolines] ...`): the page is no PE image, so its
/// "exports" are the trampolines by name ([`trampoline_exports`]).
pub const TRAMPOLINES_DLL: &str = "[nt-trampolines]";

/// The NT trampoline page's entries providers may bind, by name, as offsets
/// from the page (gex_kernel::nt::boot::tramp): the i386 unix-call
/// dispatcher. `unix_call_32` is the page offset of `tramp::UNIX_CALL_32`.
pub fn trampoline_exports(unix_call_32: u32) -> HashMap<String, u32> {
    HashMap::from([("unix_call_32".to_string(), unix_call_32)])
}

/// [`for_dll`] for an image whose file is `data`: without the `nowine`
/// providers when it is Wine's own build, the `wineonly` ones when not.
pub fn for_image(dll: &str, class32: bool, data: &[u8]) -> Vec<Provider> {
    let wine = data.get(0x40..0x50) == Some(&b"Wine builtin DLL"[..]);
    let mut v = for_dll(dll, class32);
    v.retain(|p| !(p.nowine && wine) && !(p.wineonly && !wine));
    v
}

/// A hash of `providers`' sources and what they compile against: part of
/// the key of a unit built around them, and of their own modules.
pub fn fingerprint(providers: &[Provider]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    PRELUDE.hash(&mut h);
    header::header().hash(&mut h);
    toolchain::cflags_key().hash(&mut h);
    for p in providers {
        p.name.hash(&mut h);
        p.text.hash(&mut h);
        // Only a stateful provider sees them: the keys of every other
        // provider (and so of the units built around it) do not move.
        if p.stateful {
            // As `INCLUDES.hash(h)` on a 64-bit host: a slice's length is a
            // `usize`, so it is written as a u64 for the browser's (wasm32)
            // key to match the units built natively.
            h.write_u64(INCLUDES.len() as u64);
            for i in INCLUDES {
                i.hash(&mut h);
            }
        }
    }
    format!("{:016x}", h.finish())
}

/// The guest addresses `providers` take over in an image whose exports are
/// `exports` (name -> RVA) at `base`: the bodies its unit must leave to the
/// function table.
pub fn stubs(providers: &[Provider], exports: &HashMap<String, u32>, base: u64) -> Vec<u64> {
    let mut v: Vec<u64> = providers
        .iter()
        .flat_map(|p| p.binds.iter())
        .filter_map(|(_, e)| exports.get(e).map(|&rva| base + rva as u64))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The guest addresses among [`stubs`] whose unit keeps the lifted body as
/// a twin (`lifted` providers' exports), marked `gex_abi::ftab::TWIN`: what
/// `image::build_c_module` takes beside the stubs.
pub fn twins(providers: &[Provider], exports: &HashMap<String, u32>, base: u64) -> Vec<u64> {
    let mut v: Vec<u64> = providers
        .iter()
        .filter(|p| p.lifted)
        .flat_map(|p| p.binds.iter())
        .filter_map(|(_, e)| exports.get(e).map(|&rva| (base + rva as u64) | gex_abi::ftab::TWIN))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The C symbol a link group calls for `p`'s function `f`
/// ([`group_source`]).
pub fn group_symbol(p: &Provider, f: &str) -> String {
    format!("gexg_{}_{f}", p.name)
}

/// `p`'s source as one translation unit of a link group's module
/// (docs/eject-pe.md §6a G2): no `GEX_PROVIDE` exports (the group's own
/// bodies at the exports' addresses call in), and each bound function
/// reachable from the group's units as [`group_symbol`].
pub fn group_source(p: &Provider) -> String {
    let mut s = format!(
        "#include \"gex_abi.h\"\n#include \"provider.h\"\n#undef GEX_PROVIDE\n#define GEX_PROVIDE(fn, export)\n#line 1 \"{}.c\"\n{}\n",
        p.name, p.text
    );
    let mut seen = std::collections::HashSet::new();
    for (f, _) in &p.binds {
        if seen.insert(f.as_str()) {
            s.push_str(&format!(
                "int {}(int ctx, int32_t depth, uint32_t need, uint64_t ret) {{ GEX_MUSTTAIL return {f}(ctx, depth, need, ret); }}\n",
                group_symbol(p, f)
            ));
        }
    }
    s
}

/// The prelude a provider source compiles against (`provider.h`).
pub fn prelude() -> &'static str {
    PRELUDE
}

/// The provider's module, built once into `root/providers` (keyed by its
/// [`fingerprint`] and clang's version), and the C function each slot of
/// its element segment holds.
pub fn build(p: &Provider, root: &Path) -> Result<(PathBuf, Vec<Option<String>>)> {
    let key = format!("{}-{}", p.name, fingerprint(std::slice::from_ref(p)));
    let dir = root.join("providers");
    let out = dir.join(format!("{key}.wasm"));
    if !out.exists() {
        let work = dir.join(format!(".{key}.work-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work)?;
        std::fs::write(work.join("gex_abi.h"), header::header())?;
        std::fs::write(work.join("provider.h"), PRELUDE)?;
        if p.stateful {
            for (n, t) in INCLUDES {
                std::fs::write(work.join(n), t)?;
            }
        }
        let src = work.join(format!("{}.c", p.name));
        std::fs::write(&src, format!("#include \"gex_abi.h\"\n#include \"provider.h\"\n#line 1 \"{}.c\"\n{}", p.name, p.text))?;
        let obj = work.join(format!("{}.o", p.name));
        toolchain::compile_one(&work, &src, &obj, None).with_context(|| format!("compiling provider {}", p.name))?;
        let tmp = work.join("p.wasm");
        toolchain::link_image(&work, &[obj], &tmp).with_context(|| format!("linking provider {}", p.name))?;
        // Instantiated as units are: no data (its memory base is 0) and no
        // shadow stack (its stack pointer is a dummy). A stateful one gets
        // both from the host.
        let wasm = std::fs::read(&tmp)?;
        for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
            if let wasmparser::Payload::DataSection(r) = payload? {
                if r.count() > 0 && !p.stateful {
                    bail!("provider {} has static data, which a function-table module cannot carry", p.name);
                }
            }
        }
        let users = if p.stateful { Vec::new() } else { crate::splice::unspliceable_users(&wasm)? };
        if !users.is_empty() {
            bail!("provider {}: {users:?} use clang's shadow stack", p.name);
        }
        std::fs::rename(&tmp, &out)?;
        let _ = std::fs::remove_dir_all(&work);
    }
    let wasm = std::fs::read(&out)?;
    Ok((out, slot_names(&wasm)?))
}

/// The function name in each slot of a linked module's element segment.
pub fn slot_names(wasm: &[u8]) -> Result<Vec<Option<String>>> {
    let mut slots: Vec<u32> = Vec::new();
    let mut names: HashMap<u32, String> = HashMap::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload? {
            wasmparser::Payload::ElementSection(r) => {
                for el in r {
                    if let wasmparser::ElementItems::Functions(fs) = el?.items {
                        for f in fs {
                            slots.push(f?);
                        }
                    }
                }
            }
            wasmparser::Payload::CustomSection(c) if c.name() == "name" => {
                let reader = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(c.data(), c.data_offset()));
                for n in reader.flatten() {
                    if let wasmparser::Name::Function(map) = n {
                        for naming in map.into_iter().flatten() {
                            names.insert(naming.index, naming.name.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(slots.iter().map(|f| names.get(f).cloned()).collect())
}

/// The (guest address, slot) pairs that bind `p`'s module (`slots`, from
/// [`build`]) in an image with `exports` at `base`. An export the image
/// does not have is skipped (said under `verbose`).
pub fn bindings(p: &Provider, slots: &[Option<String>], exports: &HashMap<String, u32>, base: u64, verbose: bool) -> Vec<(u64, u32)> {
    let mut out = Vec::new();
    for (f, e) in &p.binds {
        let Some(k) = slots.iter().position(|s| s.as_deref() == Some(f.as_str())) else {
            if verbose {
                eprintln!("[provider] {}: {f} is not in the module's table", p.name);
            }
            continue;
        };
        match exports.get(e) {
            Some(&rva) => out.push((base + rva as u64, k as u32)),
            None if verbose => eprintln!("[provider] {}: no export {e} at {base:#x}", p.name),
            None => {}
        }
    }
    out
}

/// An ELF library's bindings for `p` (docs/elf-providers.md §2): (guest
/// address, C function) for each export of `exports`
/// (`gex_ld::elf::Image::exports_by_version`: name -> (unbiased value,
/// IFUNC)) the source binds, the library loaded at `bias`. An IFUNC binds its
/// resolver to `gexp__ifunc` and the resolver's address plus one, which that
/// returns, to the function. A group binds whole or not at all. What is not
/// bound is said under `verbose`.
pub fn elf_bindings(p: &Provider, exports: &HashMap<String, (u64, bool)>, bias: u64, verbose: bool) -> Vec<(u64, String)> {
    let ifunc = p.text.contains("GEX_ELF_IFUNC");
    let mut by_export: HashMap<&str, Vec<(u64, String)>> = HashMap::new();
    for (f, e) in &p.binds {
        let Some(&(v, is_ifunc)) = exports.get(e) else {
            if verbose {
                eprintln!("[provider] {}: no export {e}", p.name);
            }
            continue;
        };
        let at = v.wrapping_add(bias);
        let pairs = match (is_ifunc, ifunc) {
            (false, _) => vec![(at, f.clone())],
            (true, true) => vec![(at, "gexp__ifunc".to_string()), (at + 1, f.clone())],
            (true, false) => {
                if verbose {
                    eprintln!("[provider] {}: {e} is an IFUNC and the source has no GEX_ELF_IFUNC", p.name);
                }
                continue;
            }
        };
        by_export.entry(e.as_str()).or_default().extend(pairs);
    }
    for g in &p.groups {
        if !g.iter().all(|e| by_export.contains_key(e.as_str())) {
            if verbose {
                eprintln!("[provider] {}: group {g:?} incomplete here; none of it bound", p.name);
            }
            for e in g {
                by_export.remove(e.as_str());
            }
        }
    }
    let mut out: Vec<(u64, String)> = by_export.into_values().flatten().collect();
    out.sort();
    out.dedup_by_key(|(a, _)| *a);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn elf_bindings_ifunc_and_groups() {
        let p = super::parse(
            "t",
            "/* PROVIDES libc.so.6 x86_64\nGEX_ELF_IFUNC\nGEX_GROUP(\"malloc,free\")\nGEX_PROVIDE(p_memcpy, \"memcpy\")\nGEX_PROVIDE(p_strlen, \"strlen\")\nGEX_PROVIDE(p_malloc, \"malloc\")\nGEX_PROVIDE(p_free, \"free\")\n",
        )
        .unwrap();
        let ex: super::HashMap<String, (u64, bool)> =
            [("memcpy".to_string(), (0x100, true)), ("strlen".to_string(), (0x200, false)), ("malloc".to_string(), (0x300, false))].into_iter().collect();
        let b = super::elf_bindings(&p, &ex, 0x1000, false);
        let want: Vec<(u64, String)> = vec![(0x1100, "gexp__ifunc".into()), (0x1101, "p_memcpy".into()), (0x1200, "p_strlen".into())];
        assert_eq!(b, want, "free is missing, so the malloc group binds nothing");
    }

    #[test]
    fn every_source_parses() {
        for (n, t) in super::SOURCES {
            let p = super::parse(n, t).unwrap();
            assert!(!p.binds.is_empty());
        }
    }

    /// Needs the units' toolchain: `cargo test -p gex-cgen -- --ignored providers_build`.
    #[test]
    #[ignore]
    fn providers_build() {
        let root = std::env::temp_dir().join(format!("gex-providers-build-{}", std::process::id()));
        for (n, t) in super::SOURCES {
            let p = super::parse(n, t).unwrap();
            let (path, slots) = super::build(&p, &root).unwrap_or_else(|e| panic!("{n}: {e:#}"));
            // Nothing left undefined (wasm-ld's --allow-undefined would make
            // it an import no host provides).
            for payload in wasmparser::Parser::new(0).parse_all(&std::fs::read(&path).unwrap()) {
                if let wasmparser::Payload::ImportSection(r) = payload.unwrap() {
                    for i in r {
                        let i = i.unwrap();
                        eprintln!("{n}: imports {}.{}", i.module, i.name);
                    }
                }
            }
            for (f, _) in &p.binds {
                assert!(slots.iter().any(|s| s.as_deref() == Some(f.as_str())), "{n}: {f} not in the table");
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
