//! `gexc eject build <project>` (run by the project's `build.sh`): translate
//! the program ahead of time, bind the user's replacements, and assemble the
//! guest's filesystem. Writes `build/`:
//!
//! - `build/modules/*.wasm`: one tier-1 module per program image;
//! - `build/rootfs/`: `rootfs/` with `user/files/` over it;
//! - `build/build.json`: what was built from what, which `run` reads.
//!
//! Every check fails the build rather than warn: a changed object, a stale
//! replacement, a symbol that is not there, a wasm export of the wrong type.

use crate::config::{Config, Coverage, Replace, Target, CONFIG_FILE};
use crate::eject::sha256;
use crate::json::{self, field, obj, str_field, Json};
use crate::project::{rootfs_rel, Object, Project, PROJECT_FILE};
use anyhow::{anyhow, bail, Context, Result};
use gex_native::aot::Abi;
use gex_xlate::{Intrinsic, Options};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use crate::manifest::{inputs_id, rootfs, BuildManifest, BuiltImage, BuiltReplacement, BUILD_FILE};

#[derive(Debug, Default)]
pub struct BuildReport {
    /// The web build: its modules' size, or why there is none. `None` when
    /// the project has no browser runtime.
    pub web: Option<std::result::Result<u64, String>>,
    pub images: Vec<BuiltImage>,
    pub replacements: Vec<BuiltReplacement>,
    pub illegal_blocks: usize,
    pub rootfs_files: usize,
    /// A Windows program's build (`pe`).
    pub pe: Option<crate::pe::PeReport>,
    /// A 32-bit Wine page's build.
    pub w3: Option<crate::w3::W3Report>,
    /// The libraries the placement window holds, of the closure's.
    pub placement: Placement,
}

/// Check the project's `rootfs/` is what the eject wrote.
pub(crate) fn verify_rootfs(dir: &Path, project: &Project) -> Result<()> {
    let items = project
        .objects
        .iter()
        .map(|o| (&o.path, &o.id))
        .chain(project.files.iter().map(|f| (&f.path, &f.id)));
    for (path, id) in items {
        let p = dir.join(rootfs_rel(path)?);
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        let got = sha256(&bytes);
        if &got != id {
            bail!(
                "{} changed since the eject ({got}, not {id}). rootfs/ is generated; put your \
                 own files in user/files/, or re-eject",
                p.display()
            );
        }
    }
    Ok(())
}

/// The object a replacement names: by guest path, or by file name when that
/// is unambiguous.
pub(crate) fn find_object<'a>(placed: &'a [Placed], name: &str) -> Result<&'a Placed> {
    if let Some(o) = placed.iter().find(|p| p.obj.path == name) {
        return Ok(o);
    }
    let by_name: Vec<&Placed> = placed
        .iter()
        .filter(|p| p.obj.path.rsplit('/').next() == Some(name) || p.image.soname.as_deref() == Some(name))
        .collect();
    match by_name.as_slice() {
        [o] => Ok(o),
        [] => bail!(
            "no object named {name:?} (the project's objects: {})",
            placed.iter().map(|p| p.obj.path.as_str()).collect::<Vec<_>>().join(", ")
        ),
        _ => bail!("{name:?} names several objects; give its guest path"),
    }
}

/// An object loaded where the run will place it.
pub(crate) struct Placed {
    pub(crate) obj: Object,
    pub(crate) image: gex_ld::Image,
    /// Whether the run pins it there. A library left out of the placement
    /// window is not: the run loads it wherever and translates it in each
    /// process, and its `image` is only loaded somewhere to be read.
    pub(crate) pinned: bool,
}

/// How much of the closure's libraries the placement window holds.
#[derive(Debug, Default, Clone, Copy)]
pub struct Placement {
    pub placed: usize,
    pub placed_bytes: u64,
    pub libraries: usize,
    pub library_bytes: u64,
}

/// Place every object as the run will: executables at `PIE_BASE` (gex's
/// loader), the interpreter at `INTERP_BASE` (likewise), and libraries up
/// from `MMAP_BOTTOM` in the session's order, each at a base the run pins
/// with `RunOptions::pins`. The same rule as `gexc build --closure`.
///
/// When the libraries do not all fit below `PLACE_TOP`, the window takes
/// the hottest that fit (by the blocks `profile` says each ran, then the
/// session's order) and the rest are left unpinned.
pub(crate) fn layout(
    dir: &Path,
    project: &Project,
    profile: Option<&gex_xlate::profile::Profile>,
) -> Result<(Vec<Placed>, Placement)> {
    const SLACK: u64 = 0x10000;
    let round = |n: u64| (n + SLACK - 1) & !(SLACK - 1);
    let mut loaded = Vec::new();
    for o in &project.objects {
        let p = dir.join(rootfs_rel(&o.path)?);
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        let base = match o.role.as_str() {
            "exe" => gex_abi::PIE_BASE,
            "interp" => gex_abi::INTERP_BASE,
            // Measured by loading it once at the bottom (not at zero, which
            // gex-ld refuses as the runtime's region).
            "library" => gex_abi::MMAP_BOTTOM,
            r => bail!("{}: unknown object role {r:?}", o.path),
        };
        let image = gex_ld::elf::load(&bytes, base).map_err(|e| anyhow!("loading {}: {e:?}", o.path))?;
        loaded.push((o, bytes, image));
    }
    // Each library's share of the window: its span and the slack after it.
    let share = |image: &gex_ld::Image| round(image.brk - gex_abi::MMAP_BOTTOM) + SLACK;
    let libs: Vec<usize> = (0..loaded.len()).filter(|&i| loaded[i].0.role == "library").collect();
    let window = gex_abi::PLACE_TOP - gex_abi::MMAP_BOTTOM;
    let total: u64 = libs.iter().map(|&i| share(&loaded[i].2)).sum();
    let heat = |i: usize| -> u64 {
        profile
            .and_then(|pr| pr.objects.get(&loaded[i].0.id))
            .map_or(0, |op| op.blocks.values().map(|b| b.n).sum())
    };
    let fits = fit_window(
        &libs.iter().map(|&i| (share(&loaded[i].2), heat(i))).collect::<Vec<_>>(),
        window,
    );
    let mut pinned = vec![true; loaded.len()];
    for (k, &i) in libs.iter().enumerate() {
        pinned[i] = fits[k];
    }
    let mut placement = Placement { libraries: libs.len(), library_bytes: total, ..Placement::default() };
    let mut next = gex_abi::MMAP_BOTTOM;
    let mut out = Vec::new();
    for (i, (o, bytes, image)) in loaded.into_iter().enumerate() {
        let image = if o.role == "library" && pinned[i] {
            let base = round(next);
            next = base + share(&image);
            placement.placed += 1;
            placement.placed_bytes += share(&image);
            gex_ld::elf::load(&bytes, base).map_err(|e| anyhow!("loading {}: {e:?}", o.path))?
        } else {
            image
        };
        out.push(Placed { obj: o.clone(), image, pinned: pinned[i] });
    }
    Ok((out, placement))
}

/// Which libraries, given as (window share, blocks run) in the session's
/// order, the window holds: all when they fit, else the hottest that fit
/// (a tie by the session's order), each one that would overflow skipped for
/// the next.
fn fit_window(libs: &[(u64, u64)], window: u64) -> Vec<bool> {
    if libs.iter().map(|l| l.0).sum::<u64>() <= window {
        return vec![true; libs.len()];
    }
    let mut by_heat: Vec<usize> = (0..libs.len()).collect();
    by_heat.sort_by_key(|&i| (std::cmp::Reverse(libs[i].1), i));
    let mut fits = vec![false; libs.len()];
    let mut used = 0;
    for i in by_heat {
        if used + libs[i].0 <= window {
            used += libs[i].0;
            fits[i] = true;
        }
    }
    fits
}

/// The bytes of an image at `[addr, addr + len)`, if they are all loaded.
fn code_bytes(image: &gex_ld::Image, addr: u64, len: u64) -> Option<&[u8]> {
    image.segments.iter().find_map(|s| {
        let off = addr.checked_sub(s.vaddr)? as usize;
        s.bytes.get(off..off.checked_add(len as usize)?)
    })
}

/// `sha256:` of a function's bytes, as `bytes` in a replacement names it.
pub fn function_hash(image: &gex_ld::Image, addr: u64, size: u64) -> Result<String> {
    if size == 0 {
        bail!("the function at {addr:#x} has no size (give one)");
    }
    let b = code_bytes(image, addr, size)
        .ok_or_else(|| anyhow!("[{addr:#x}, {:#x}) is not all in the object's loaded bytes", addr + size))?;
    Ok(sha256(b))
}

/// The unique function a symbol names in a placed image: `(address, size)`.
pub(crate) fn symbol_in(p: &Placed, symbol: &str) -> Result<(u64, u64)> {
    let (tlo, thi) = p.image.text_range;
    let mut found: Vec<(u64, u64, bool)> = p
        .image
        .symbols
        .iter()
        .filter(|s| s.name == symbol)
        .map(|s| (s.value, s.size, s.ifunc)) // gex-ld biases symbol values
        .filter(|(a, _, _)| *a >= tlo && *a < thi)
        .collect();
    found.sort_unstable();
    found.dedup();
    match found.as_slice() {
        [(a, n, false)] => Ok((*a, *n)),
        [(_, _, true)] => bail!(
            "`{symbol}` in {} is an IFUNC (its address is a resolver, not the function): replace \
             its import, or the implementation the resolver picks",
            p.obj.path
        ),
        [] => bail!("{} defines no function `{symbol}` (is it stripped? use \"offset\")", p.obj.path),
        _ => bail!("{} defines `{symbol}` at several addresses", p.obj.path),
    }
}

/// Resolve and check every replacement. Returns them numbered, and the
/// replacement block for each placed address.
fn bind_replacements(
    dir: &Path,
    placed: &[Placed],
    config: &Config,
) -> Result<(Vec<BuiltReplacement>, HashMap<u64, Intrinsic>)> {
    let mut out: Vec<BuiltReplacement> = Vec::new();
    let mut at: HashMap<u64, (u32, String)> = HashMap::new();
    let mut modules: HashMap<String, gex_native::aot::ReplacementModule> = HashMap::new();
    for (i, r) in config.replace.iter().enumerate() {
        let Replace { object, sha256: want, bytes, target, wasm, export, abi } = r;
        let what = match target {
            Target::Symbol(n) => format!("{CONFIG_FILE}: replace[{i}] ({n} in {object})"),
            Target::Offset { offset, .. } => format!("{CONFIG_FILE}: replace[{i}] ({object} + {offset:#x})"),
            Target::Import(n) if object.is_empty() => format!("{CONFIG_FILE}: replace[{i}] (every import of {n})"),
            Target::Import(n) => format!("{CONFIG_FILE}: replace[{i}] ({object}'s import of {n})"),
            Target::Library { .. } => format!("{CONFIG_FILE}: replace[{i}] (the library {object})"),
        };
        let module = match modules.get(wasm) {
            Some(m) => m,
            None => {
                let b = std::fs::read(dir.join(wasm)).with_context(|| {
                    if wasm == crate::config::USER_WASM {
                        format!("{what}: no {wasm}; build.sh compiles user/src/*.c into it")
                    } else {
                        format!("{what}: reading {wasm}")
                    }
                })?;
                let m = gex_native::aot::ReplacementModule::inspect(&b).with_context(|| format!("{what}: {wasm}"))?;
                modules.entry(wasm.clone()).or_insert(m)
            }
        };
        // An "entry" replacement is a whole function, return included: it
        // stands for a definition, not an import or a library's calls.
        if *abi == Abi::Entry && !matches!(target, Target::Symbol(_) | Target::Offset { .. }) {
            bail!("{what}: the \"entry\" ABI replaces a function by \"symbol\" or \"offset\"");
        }
        // Replacement blocks take the 64-bit calling convention (arguments
        // in registers, an 8-byte return address); an i386 object's cdecl is
        // not that, so binding one there would call it with garbage.
        if let Some(p) = placed.iter().find(|p| p.image.class32 && (object.is_empty() || p.obj.path == *object || p.obj.path.rsplit('/').next() == Some(object.as_str()))) {
            bail!("{what}: {} is 32-bit (i386) code, which replacements do not support yet", p.obj.path);
        }
        // The object(s), each checked against the hash it was written for.
        let objs: Vec<&Placed> = if object.is_empty() {
            if want.is_some() {
                bail!("{what}: a \"sha256\" needs the \"object\" it is the hash of");
            }
            placed.iter().filter(|p| p.pinned).collect()
        } else {
            let p = find_object(placed, object).with_context(|| what.clone())?;
            if !p.pinned {
                bail!("{what}: {} is not in the placement window (the closure's hottest libraries are), so nothing translates it ahead of time", p.obj.path);
            }
            if let Some(want) = want {
                if *want != p.obj.id {
                    bail!(
                        "{what} is stale: it was written for {want}, and the project's {} is now {}. \
                         Check the replacement still fits and update its \"sha256\"",
                        p.obj.path,
                        p.obj.id
                    );
                }
            }
            vec![p]
        };
        // `(object, address, name, export, missing)` to bind.
        let mut binds: Vec<(&Placed, u64, String, String, bool)> = Vec::new();
        match target {
            Target::Symbol(_) | Target::Offset { .. } => {
                let p = objs[0];
                let (addr, size, name) = match target {
                    Target::Symbol(n) => {
                        let (a, sz) = symbol_in(p, n).with_context(|| what.clone())?;
                        (a, sz, n.clone())
                    }
                    Target::Offset { offset, size } => {
                        let a = offset.wrapping_add(p.image.bias);
                        let (tlo, thi) = p.image.text_range;
                        if a < tlo || a >= thi {
                            bail!("{what}: {offset:#x} is not in {}'s text", p.obj.path);
                        }
                        (a, *size, format!("{}+{offset:#x}", p.obj.path.rsplit('/').next().unwrap_or("")))
                    }
                    _ => unreachable!(),
                };
                if let Some(want) = bytes {
                    let got = function_hash(&p.image, addr, size).with_context(|| what.clone())?;
                    if &got != want {
                        bail!(
                            "{what} is stale: the function's bytes are now {got}, not the {want} it \
                             was written for. Check the replacement still fits and update \"bytes\""
                        );
                    }
                }
                binds.push((p, addr, name, export.clone(), false));
            }
            Target::Import(n) => {
                if bytes.is_some() {
                    bail!("{what}: \"bytes\" names a function's code; an import has none");
                }
                for p in &objs {
                    for (a, name) in crate::plt::stubs(&p.image) {
                        if &name == n {
                            binds.push((p, a, format!("{}'s import of {n}", p.obj.path), export.clone(), false));
                        }
                    }
                }
                if binds.is_empty() {
                    bail!("{what}: no PLT stub calls {n} (a call through the GOT without a PLT is not caught)");
                }
            }
            Target::Library { prefix } => {
                if bytes.is_some() {
                    bail!("{what}: \"bytes\" names one function's code, not a library's");
                }
                let p = objs[0];
                if p.obj.role != "library" {
                    bail!("{what}: {} is the {}, not a library", p.obj.path, p.obj.role);
                }
                let (tlo, thi) = p.image.text_range;
                let mut ifuncs = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                for d in p.image.dynsyms.iter().filter(|d| !d.undefined && d.value != 0) {
                    let a = d.value.wrapping_add(p.image.bias);
                    if a < tlo || a >= thi || !seen.insert(a) {
                        continue;
                    }
                    if p.image.symbols.iter().any(|s| s.value == a && s.ifunc) {
                        ifuncs.push(d.name.clone());
                        continue;
                    }
                    let e = format!("{prefix}{}", d.name);
                    let missing = !module.has(&e);
                    binds.push((p, a, d.name.clone(), e, missing));
                }
                if !ifuncs.is_empty() {
                    bail!("{what}: {} exports IFUNCs ({}), which a library replacement cannot bind", p.obj.path, ifuncs.join(", "));
                }
                if binds.iter().all(|b| b.4) {
                    bail!("{what}: {wasm} provides none of {}'s {} functions", p.obj.path, binds.len());
                }
            }
        }
        for (p, addr, name, exp, missing) in binds {
            let sig = if missing { String::new() } else { module.check(&exp, *abi).with_context(|| format!("{what}: {wasm}"))? };
            if let Some((other, oname)) = at.get(&addr) {
                bail!("{what}: {name} at {addr:#x} is already replaced ({oname}, by replace[{other}])");
            }
            at.insert(addr, (i as u32, name.clone()));
            out.push(BuiltReplacement {
                id: out.len() as u32,
                rule: i as u32,
                kind: match target {
                    Target::Symbol(_) => "symbol",
                    Target::Offset { .. } => "offset",
                    Target::Import(_) => "import",
                    Target::Library { .. } => "library",
                }
                .to_string(),
                missing,
                module: wasm.clone(),
                export: exp,
                abi: *abi,
                object: p.obj.id.clone(),
                symbol: name,
                addr,
                sig,
            });
        }
    }
    let map = out
        .iter()
        .map(|r| {
            let k = if r.abi == Abi::Entry { Intrinsic::Override(r.id) } else { Intrinsic::Replace(r.id) };
            (r.addr, k)
        })
        .collect();
    Ok((out, map))
}

/// A checked `user/eject.json` entry for a function: its object's hash and
/// its bytes' hash filled in. `what` is a symbol or a `0x` link-time offset.
pub fn describe(dir: &Path, object: &str, what: &str, size: Option<u64>) -> Result<String> {
    let project = Project::load(dir)?;
    let (placed, _) = layout(dir, &project, None)?;
    let p = find_object(&placed, object)?;
    let name = p.obj.path.rsplit('/').next().unwrap_or(&p.obj.path).to_string();
    let (kv, addr, size) = if let Some(h) = what.strip_prefix("0x") {
        let off = u64::from_str_radix(h, 16).map_err(|_| anyhow!("{what} is not a hex offset"))?;
        let addr = off.wrapping_add(p.image.bias);
        let size = match size {
            Some(s) => s,
            None => p
                .image
                .symbols
                .iter()
                .find(|s| s.value == addr && s.size > 0)
                .map(|s| s.size)
                .ok_or_else(|| anyhow!("no symbol gives the size of the function at {what}: pass --size"))?,
        };
        (
            vec![("offset", json::s(format!("{off:#x}"))), ("size", json::s(format!("{size:#x}")))],
            addr,
            size,
        )
    } else {
        let (a, sz) = symbol_in(p, what)?;
        let sz = size.unwrap_or(sz);
        (vec![("symbol", json::s(what))], a, sz)
    };
    let mut fields = vec![("object", json::s(&name)), ("sha256", json::s(&p.obj.id))];
    fields.extend(kv);
    fields.push(("bytes", json::s(function_hash(&p.image, addr, size)?)));
    let export = match what.strip_prefix("0x") {
        Some(h) => format!("fn_{h}"),
        None => what.to_string(),
    };
    fields.push(("export", json::s(export)));
    fields.push(("abi", json::s("c")));
    Ok(json::write(&obj(fields)))
}

pub(crate) fn copy_tree(from: &Path, to: &Path, count: &mut usize) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let mut entries: Vec<_> = std::fs::read_dir(from)
        .with_context(|| format!("reading {}", from.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let src = e.path();
        let dst = to.join(e.file_name());
        let t = e.file_type()?;
        if t.is_dir() {
            copy_tree(&src, &dst, count)?;
        } else if t.is_symlink() {
            let target = std::fs::read_link(&src)?;
            let _ = std::fs::remove_file(&dst);
            std::os::unix::fs::symlink(&target, &dst)
                .with_context(|| format!("linking {}", dst.display()))?;
        } else {
            if dst.is_dir() {
                bail!("{} is a file over a directory of the rootfs", src.display());
            }
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
            crate::eject::keep_mtime(&dst, &e.metadata()?)?;
            *count += 1;
        }
    }
    Ok(())
}

/// Everything translation needs, for the native and the web module sets.
struct Translation<'a> {
    dir: &'a Path,
    project: &'a Project,
    placed: &'a [Placed],
    profile: Option<&'a gex_xlate::profile::Profile>,
    config: &'a Config,
    intrinsics: &'a HashMap<u64, Intrinsic>,
}

impl Translation<'_> {
    /// Translate every object into `out` (relative to the project): the
    /// native set, or (`web`) the browser shim's. Returns the modules, the
    /// libraries' pins and the count of blocks ending in SIGILL.
    /// The programs' image modules (none when `programs` is false: they are
    /// units, `units`), and the pins every run places the libraries at.
    fn translate(&self, out: &str, web: bool, programs: bool) -> Result<(Vec<BuiltImage>, Vec<(String, u64)>, usize)> {
        // The program's closure is numbered as one: its image owns the memory and
        // tables, and every other object's slots continue where the last one's
        // stopped. Other executables (programs the session exec'd) are images of
        // their own.
        let main = self.placed
            .iter()
            .position(|p| p.obj.path == self.project.program.exe && p.obj.role == "exe")
            .ok_or_else(|| anyhow!("the project's program {} is not among its objects", self.project.program.exe))?;
        let mut order: Vec<usize> = vec![main];
        // (a placed library, and the interpreter, is a unit of its own:
        // `units`; one the window left out is translated by the run, in
        // each process)
        let dynamic = self.placed.iter().any(|p| p.obj.role != "exe");
        let others: Vec<usize> = (0..self.placed.len()).filter(|&i| i != main && self.placed[i].obj.role == "exe").collect();
        let main_identity = gex_native::aot::image_identity(&self.placed[main].image);

        let mut images = Vec::new();
        let mut illegal = 0;
        let mut pins = Vec::new();
        let mut first_slot = 0u64;
        for (n, &i) in order.iter().chain(others.iter()).enumerate().filter(|_| programs) {
            let p = &self.placed[i];
            let o = &p.obj;
            let in_closure = n < order.len();
            // The browser runs a dynamic program's closure the way gex's own
            // pages do (`gexc build --closure --shared-image`): every object
            // is a demand-filled library module, including the executable, and
            // the process starts from a seed module. An image module writing
            // its slots at instantiation does not mix with the pages the shim
            // numbers for itself.
            let shared_image = web && dynamic;
            let own = !shared_image && (n == 0 || !in_closure);
            let mut image = p.image.clone();
            gex_xlate::set_compat(image.class32);
            if !own {
                // Apply its own RELATIVE relocations first, so the pointer scan
                // follows them into the code they name (as `gexc build --closure`).
                gex_ld::elf::apply_own_relative(&mut image);
            }
            let (tlo, thi) = image.text_range;
            let seeds: Vec<u64> = self
                .profile
                .and_then(|pr| pr.objects.get(&o.id))
                .map(|op| {
                    op.blocks
                        .keys()
                        .map(|off| off.wrapping_add(image.bias))
                        .filter(|a| *a >= tlo && *a < thi)
                        .collect()
                })
                .unwrap_or_default();
            // `profile`: the reached blocks and their pages, and nothing followed
            // off them onto fresh pages (the profile-guided closure build's
            // rule). `static` adds discovery from the symbols and the data.
            // A session recorded without a profile says nothing about what ran:
            // then `profile` is `static`.
            let profile_only = self.config.coverage == Coverage::Profile && self.profile.is_some();
            let opts = Options {
                exhaustive: self.config.coverage == Coverage::Exhaustive,
                lazy: profile_only,
                scan_pointers: !profile_only,
                seeds_only: profile_only,
                extra_entries: seeds,
                complete_pages: self.config.coverage != Coverage::Exhaustive,
                intrinsics: self.intrinsics
                    .iter()
                    .filter(|(a, _)| **a >= tlo && **a < thi)
                    .map(|(a, k)| (*a, *k))
                    .collect(),
                // The browser's single-Worker shim takes a plain memory, and
                // fills a library's pages on demand from passive segments.
                shared_memory: !web,
                demand_fill: web && !own,
                linkage: if own { gex_xlate::Linkage::Own } else { gex_xlate::Linkage::Shared },
                first_slot: if own || n == 0 { 0 } else { first_slot },
                ..Options::default()
            };
            let (modules, stats) = gex_xlate::translate_image_split(&image, &opts, gex_xlate::MAX_FUNCS_PER_MODULE);
            if in_closure {
                first_slot = modules.last().map_or(first_slot, |m| m.metadata().next_slot);
            }
            let name = o.path.rsplit('/').next().unwrap_or("image");
            let hex = o.id.trim_start_matches("sha256:");
            let identity = if in_closure { main_identity.clone() } else { gex_native::aot::image_identity(&image) };
            for (part, module) in modules.iter().enumerate() {
                gex_native::validate_module(&module.wasm)
                    .with_context(|| format!("the translation of {} is not valid wasm", o.path))?;
                let rel = if modules.len() == 1 {
                    format!("{out}/{name}-{}.wasm", &hex[..12.min(hex.len())])
                } else {
                    format!("{out}/{name}-{}.p{part}.wasm", &hex[..12.min(hex.len())])
                };
                std::fs::write(self.dir.join(&rel), &module.wasm).with_context(|| format!("writing {rel}"))?;
                images.push(BuiltImage {
                    kind: if own { "image" } else { "shared" }.to_string(),
                    identity: identity.clone(),
                    base: image.bias,
                    module: rel,
                    object: o.id.clone(),
                    path: o.path.clone(),
                    blocks: if part == 0 { stats.blocks as u64 } else { 0 },
                    bytes: module.wasm.len() as u64,
                });
            }
            illegal += stats.illegal_blocks;
            if o.role == "library" {
                pins.push((o.path.clone(), image.bias));
            }
        }
        pins.extend(self.placed.iter().filter(|p| p.obj.role == "library" && p.pinned).map(|p| (p.obj.path.clone(), p.image.bias)));
        Ok((images, pins, illegal))
    }

    /// The function-tier unit (`gexc cgen-image --elf`) of each placed
    /// library and of the interpreter, into `out` (relative to the project).
    /// A unit is keyed by its object as placed, not by any program, so every
    /// process that maps the object at its base runs it. The native flavour
    /// (no bounds guard), which the page runs as well.
    fn units(&self, out: &str, programs: bool) -> Result<Vec<BuiltImage>> {
        let cache = self.dir.join(out);
        std::fs::create_dir_all(&cache).with_context(|| format!("creating {}", cache.display()))?;
        let jobs = std::env::var("GEX_ELF_UNITS_JOBS")
            .ok()
            .and_then(|j| j.parse().ok())
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get().min(8)));
        let unguarded = gex_cgen::image::unguarded();
        gex_native::pe_units::set_cache_root(Some(cache.clone()));
        gex_cgen::image::set_unguarded(true);
        let built = (|| {
            let mut images = Vec::new();
            // Programs too, at the base gex's loader gives them (PIE_BASE):
            // one function tier for all of a Linux program's code.
            for p in self.placed.iter().filter(|p| (programs && p.obj.role == "exe") || (p.pinned && (p.obj.role == "library" || p.obj.role == "interp"))) {
                let host = self.dir.join(rootfs_rel(&p.obj.path)?);
                let bytes = std::fs::read(&host).with_context(|| format!("reading {}", host.display()))?;
                let wasm = gex_native::pe_units::prepare_elf(&bytes, p.image.bias, jobs, false)
                    .with_context(|| format!("the function-tier unit of {} at {:#x}", p.obj.path, p.image.bias))?;
                let key = wasm.parent().and_then(|d| d.file_name()).map(|k| k.to_string_lossy().into_owned()).unwrap_or_default();
                let entries = gex_cgen::image::read_entries(&wasm)?.iter().filter(|&&a| a != 0).count() as u64;
                images.push(BuiltImage {
                    kind: "elf-unit".to_string(),
                    identity: key.clone(),
                    base: p.image.bias,
                    module: format!("{out}/{key}/c.wasm"),
                    object: p.obj.id.clone(),
                    path: p.obj.path.clone(),
                    blocks: entries,
                    bytes: std::fs::metadata(&wasm)?.len(),
                });
            }
            Ok(images)
        })();
        gex_native::pe_units::set_cache_root(None);
        gex_cgen::image::set_unguarded(unguarded);
        built
    }
}

pub fn build(dir: &Path) -> Result<BuildReport> {
    if crate::w3::is_project(dir) {
        return Ok(BuildReport { w3: Some(crate::w3::build(dir)?), ..BuildReport::default() });
    }
    let project = Project::load(dir)?;
    if project.isa != gex_abi::ARCH_NAME {
        bail!("the project is for {} guests and this gexc runs {} guests", project.isa, gex_abi::ARCH_NAME);
    }
    let (config, _) = Config::load(dir)?;
    if !config.features_off.is_empty() {
        crate::features::check(&config.features_off)?;
        bail!(
            "user/eject.json: features are switched off ({}), and only a W3 page's build \
             (docs/eject.md, \"W3 pages\") can leave them out so far",
            config.features_off.join(", ")
        );
    }
    if crate::pe::nt_session(dir)?.is_some() {
        let pe = crate::pe::build(dir, &project, &config)?;
        let rootfs_files = pe.rootfs_files;
        return Ok(BuildReport { pe: Some(pe), rootfs_files, ..BuildReport::default() });
    }
    verify_rootfs(dir, &project)?;
    // What the session ran, per object, to seed discovery with.
    let profile = match std::fs::read_to_string(dir.join("session/profile.json")) {
        Ok(t) => Some(gex_xlate::profile::Profile::from_json(&t).map_err(|e| anyhow!("session/profile.json: {e}"))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow!("reading session/profile.json: {e}")),
    };

    let (placed, placement) = layout(dir, &project, profile.as_ref())?;
    if placement.placed < placement.libraries && !config.jit_fallback {
        bail!(
            "the closure's {} libraries ({} MiB) do not fit the {} MiB placement window, which takes \
             the {} hottest; the rest are translated as they run, so set \"jit_fallback\": true in {CONFIG_FILE}",
            placement.libraries,
            placement.library_bytes >> 20,
            (gex_abi::PLACE_TOP - gex_abi::MMAP_BOTTOM) >> 20,
            placement.placed
        );
    }
    let (replacements, replaced) = bind_replacements(dir, &placed, &config)?;

    let out = dir.join("build");
    let modules_dir = out.join("modules");
    for d in [&modules_dir, &out.join("rootfs")] {
        if d.exists() {
            std::fs::remove_dir_all(d).with_context(|| format!("removing {}", d.display()))?;
        }
    }
    let _ = std::fs::remove_file(dir.join(BUILD_FILE));
    std::fs::create_dir_all(&modules_dir).with_context(|| format!("creating {}", modules_dir.display()))?;

    // gex's own library intrinsics (the `mem*` functions), by placed address
    // over the whole closure since a call resolves to another object's
    // definition, as a closure build and the runtime translator use them.
    let mut intrinsics: HashMap<u64, Intrinsic> = HashMap::new();
    for p in placed.iter().filter(|p| p.pinned) {
        // Not for 32-bit (i386) objects: the intrinsic block reads the 64-bit
        // register arguments and pops an 8-byte return address, and i386
        // cdecl passes them on the stack.
        if p.image.class32 {
            continue;
        }
        let (tlo, thi) = p.image.text_range;
        for s in &p.image.symbols {
            let a = s.value; // gex-ld biases symbol values
            if s.ifunc || a < tlo || a >= thi {
                continue;
            }
            if let Some(k) = gex_xlate::intrinsic::recognise(&s.name) {
                intrinsics.insert(a, k);
            }
        }
    }
    // Then the user's replacements over them.
    intrinsics.extend(replaced);

    let mut report = BuildReport { placement, ..BuildReport::default() };
    let t = Translation { dir, project: &project, placed: &placed, profile: profile.as_ref(), config: &config, intrinsics: &intrinsics };
    // The programs are function-tier units too, unless the user replaces
    // functions in them: a replacement is made in an image module's
    // translation (`intrinsics`), which a unit does not take.
    let program_units = replacements.is_empty();
    let (mut images, pins, illegal) = t.translate("build/modules", false, !program_units)?;
    let units = t.units("build/cgen", program_units)?;
    if !units.is_empty() && !config.jit_fallback {
        // Recovery leaves a function it could not delimit to the runtime
        // translator, which a run without it does not have.
        eprintln!(
            "gex-eject: warning: {} units (libraries, programs) leave what recovery missed to runtime translation; \
             a run without \"jit_fallback\" stops at the first such function",
            units.len()
        );
    }
    images.extend(units);
    report.images = images;
    report.illegal_blocks = illegal;

    // The guest's filesystem: what the session read, then the user's files.
    let mut n = 0;
    copy_tree(&dir.join("rootfs"), &out.join("rootfs"), &mut n)?;
    let user_files = dir.join("user/files");
    if user_files.is_dir() {
        copy_tree(&user_files, &out.join("rootfs"), &mut n)?;
    }
    report.rootfs_files = n;

    // The web build, when the project has the browser runtime.
    report.web = if dir.join(crate::web::SHIM).exists() {
        let web_dir = dir.join("build/web");
        if web_dir.exists() {
            std::fs::remove_dir_all(&web_dir).with_context(|| format!("removing {}", web_dir.display()))?;
        }
        std::fs::create_dir_all(&web_dir)?;
        match crate::web::unsupported(&project) {
            Some(why) => {
                crate::web::unsupported_manifest(dir, &why)?;
                Some(Err(why))
            }
            None => {
                for r in replacements.iter().filter(|r| !r.missing) {
                    crate::web::check_module(dir, &r.module, &r.export, r.abi)?;
                }
                let (mut images, _, _) = t.translate("build/web", true, true)?;
                images.extend(report.images.iter().filter(|i| i.kind == "elf-unit").cloned());
                let spans: Vec<(String, u64, u64)> = placed
                    .iter()
                    .filter(|p| p.obj.role == "library" && p.pinned)
                    .map(|p| (p.obj.path.clone(), p.image.bias, p.image.brk - p.image.bias))
                    .collect();
                let allowed = crate::run::allowed_syscalls(dir, &config)?;
                crate::web::manifest(dir, &project, &images, &spans, &replacements, allowed.as_ref(), config.jit_fallback)?;
                Some(Ok(images.iter().map(|i| i.bytes).sum()))
            }
        }
    } else {
        None
    };

    let modules: Vec<String> = replacements.iter().filter(|r| !r.missing).map(|r| r.module.clone()).collect();
    let manifest = BuildManifest {
        inputs: inputs_id(dir, &modules)?,
        images: report.images.clone(),
        replacements: replacements.clone(),
        pins,
    };
    std::fs::write(dir.join(BUILD_FILE), manifest.to_json()).with_context(|| format!("writing {BUILD_FILE}"))?;
    report.replacements = replacements;
    Ok(report)
}



#[cfg(test)]
mod tests {
    use super::fit_window;

    #[test]
    fn the_window_takes_the_hottest_libraries_that_fit() {
        // all fit: all placed, whatever ran
        assert_eq!(fit_window(&[(4, 0), (4, 9)], 8), vec![true, true]);
        // the hottest first; one that would overflow is skipped for a smaller one
        assert_eq!(fit_window(&[(4, 1), (6, 9), (3, 5), (2, 0)], 10), vec![false, true, true, false]);
        assert_eq!(fit_window(&[(4, 1), (6, 9), (5, 5), (4, 0)], 10), vec![true, true, false, false]);
        // no profile: the session's order
        assert_eq!(fit_window(&[(4, 0), (4, 0), (4, 0)], 8), vec![true, true, false]);
    }
}
