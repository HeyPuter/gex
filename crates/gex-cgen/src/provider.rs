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
];

/// One provider source, parsed.
#[derive(Clone, Debug)]
pub struct Provider {
    pub name: &'static str,
    text: &'static str,
    /// The libraries, lower case (`msvcr120.dll`).
    pub dlls: Vec<String>,
    pub class32: bool,
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
        bail!("provider {name}: the first line must be `/* PROVIDES <dll>[,<dll>...] <i386|x86_64>`");
    }
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
    Ok(Provider { name, text, dlls, class32, binds })
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
        let src = work.join(format!("{}.c", p.name));
        std::fs::write(&src, format!("#include \"gex_abi.h\"\n#include \"provider.h\"\n#line 1 \"{}.c\"\n{}", p.name, p.text))?;
        let obj = work.join(format!("{}.o", p.name));
        toolchain::compile_one(&work, &src, &obj, None).with_context(|| format!("compiling provider {}", p.name))?;
        let tmp = work.join("p.wasm");
        toolchain::link_image(&work, &[obj], &tmp).with_context(|| format!("linking provider {}", p.name))?;
        // Instantiated as units are: no data (its memory base is 0) and no
        // shadow stack (its stack pointer is a dummy).
        let wasm = std::fs::read(&tmp)?;
        for payload in wasmparser::Parser::new(0).parse_all(&wasm) {
            if let wasmparser::Payload::DataSection(r) = payload? {
                if r.count() > 0 {
                    bail!("provider {} has static data, which a function-table module cannot carry", p.name);
                }
            }
        }
        let users = crate::splice::unspliceable_users(&wasm)?;
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
}
