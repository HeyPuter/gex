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
//!   other paths ([`for_image`]).
//!
//! `GEX_PROVIDERS=0` turns every provider off (A/B runs, lockstep);
//! `GEX_PROVIDER_SKIP=<export>[,<export>...]` leaves those exports to the
//! guest's own code.

use crate::{header, toolchain};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

const PRELUDE: &str = include_str!("../providers/provider.h");

/// Every provider source gex carries.
const SOURCES: &[(&str, &str)] = &[
    ("msvcr120_concrt", include_str!("../providers/msvcr120_concrt.c")),
    ("crt_mem", include_str!("../providers/crt_mem.c")),
    ("crt_math", include_str!("../providers/crt_math.c")),
    ("debug_string", include_str!("../providers/debug_string.c")),
    ("crt_heap", include_str!("../providers/crt_heap.c")),
    ("nt_qpc", include_str!("../providers/nt_qpc.c")),
    ("nt_unixcall", include_str!("../providers/nt_unixcall.c")),
    ("msvcp120_thrd", include_str!("../providers/msvcp120_thrd.c")),
];

/// Files a stateful provider may `#include` (written beside it when built).
const INCLUDES: &[(&str, &str)] = &[("dlmalloc.inc", include_str!("../providers/dlmalloc.inc"))];

/// One provider source, parsed.
#[derive(Clone, Debug)]
pub struct Provider {
    pub name: &'static str,
    text: &'static str,
    /// The libraries, lower case (`msvcr120.dll`).
    pub dlls: Vec<String>,
    pub class32: bool,
    /// Has data and a stack (`stateful` on the first line).
    pub stateful: bool,
    /// Not for Wine's build of the library (`nowine` on the first line).
    pub nowine: bool,
    /// (C function, export), in source order.
    pub binds: Vec<(String, String)>,
}

/// Every provider gex carries (`gexc cgen-image --providers` builds them
/// all into a unit cache, for a host that cannot build them at run time).
pub fn all() -> Vec<Provider> {
    SOURCES.iter().map(|(n, t)| parse(n, t).expect("a provider source gex carries is well formed")).collect()
}

/// Whether providers are on (`GEX_PROVIDERS=0` turns them off).
pub fn enabled() -> bool {
    std::env::var("GEX_PROVIDERS").map_or(true, |v| v != "0")
}

fn parse(name: &'static str, text: &'static str) -> Result<Provider> {
    let first = text.lines().next().unwrap_or("");
    let w: Vec<&str> = first.split_whitespace().collect();
    if w.len() < 4 || w[0] != "/*" || w[1] != "PROVIDES" {
        bail!("provider {name}: the first line must be `/* PROVIDES <dll>[,<dll>...] <i386|x86_64> [stateful] [nowine]`");
    }
    let stateful = w[4..].contains(&"stateful");
    let nowine = w[4..].contains(&"nowine");
    let class32 = match w[3] {
        "i386" => true,
        "x86_64" => false,
        a => bail!("provider {name}: unknown architecture {a}"),
    };
    let mut binds = Vec::new();
    for l in text.lines() {
        let Some(rest) = l.trim().strip_prefix("GEX_PROVIDE(") else { continue };
        let (f, e) = rest
            .split_once(',')
            .ok_or_else(|| anyhow!("provider {name}: malformed {l:?}"))?;
        let e = e.trim().trim_end_matches(')').trim().trim_matches('"');
        binds.push((f.trim().to_string(), e.to_string()));
    }
    if binds.is_empty() {
        bail!("provider {name} provides nothing");
    }
    let skip = std::env::var("GEX_PROVIDER_SKIP").unwrap_or_default();
    binds.retain(|(_, e)| !skip.split(',').any(|s| s == e));
    let dlls = w[2].split(',').map(|d| d.to_ascii_lowercase()).collect();
    Ok(Provider { name, text, dlls, class32, stateful, nowine, binds })
}

/// The providers for library `dll` (its export directory's name, any case)
/// of the given class, when providers are on.
pub fn for_dll(dll: &str, class32: bool) -> Vec<Provider> {
    if !enabled() || !gex_abi::X86 {
        return Vec::new();
    }
    let dll = dll.to_ascii_lowercase();
    SOURCES
        .iter()
        .map(|(n, t)| parse(n, t).expect("a provider source gex carries is well formed"))
        .filter(|p| p.dlls.contains(&dll) && p.class32 == class32)
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
/// providers when it is Wine's own build.
pub fn for_image(dll: &str, class32: bool, data: &[u8]) -> Vec<Provider> {
    let wine = data.get(0x40..0x50) == Some(&b"Wine builtin DLL"[..]);
    let mut v = for_dll(dll, class32);
    v.retain(|p| !(p.nowine && wine));
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

#[cfg(test)]
mod tests {
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
