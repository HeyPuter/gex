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

pub const BUILD_FILE: &str = "build/build.json";
const FORMAT: &str = "gex-eject-build";
const VERSION: u64 = 1;

/// One translated module: a program image, or a part of a dynamic
/// program's closure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltImage {
    /// `image`: a program's own module, looked up by `identity`. `shared`: a
    /// module of the closure of the program whose identity is `identity`
    /// (its interpreter or a library), installed before it starts.
    pub kind: String,
    /// `gex_native::aot::image_identity` of the program's loaded image.
    pub identity: String,
    /// Where the object is placed: its load bias.
    pub base: u64,
    /// Relative to the project.
    pub module: String,
    /// The object it was built from.
    pub object: String,
    pub path: String,
    pub blocks: u64,
    pub bytes: u64,
}

/// One bound replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltReplacement {
    /// Its number in the `Replace` block.
    pub id: u32,
    /// The `user/eject.json` entry it came from (one entry can bind many:
    /// a library, an import of every object).
    pub rule: u32,
    /// `symbol`, `offset`, `import` or `library`.
    pub kind: String,
    /// A replaced library's function the module does not provide: it fails
    /// loudly when called. `module` and `export` name what was looked for.
    pub missing: bool,
    /// Relative to the project.
    pub module: String,
    pub export: String,
    pub abi: Abi,
    pub object: String,
    pub symbol: String,
    pub addr: u64,
    /// The export's wasm signature.
    pub sig: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildManifest {
    /// What `run` checks the project against: see [`inputs_id`].
    pub inputs: String,
    pub images: Vec<BuiltImage>,
    pub replacements: Vec<BuiltReplacement>,
    /// Where each library must be placed, by guest path: `RunOptions::pins`.
    pub pins: Vec<(String, u64)>,
}

/// The hash of everything a build is made from that a user can change
/// without rebuilding: the project, the settings, and the replacement
/// modules. `run` refuses a build whose inputs have moved on.
pub fn inputs_id(dir: &Path, modules: &[String]) -> Result<String> {
    let mut all = Vec::new();
    let mut add = |name: &str, bytes: &[u8]| {
        all.extend_from_slice(name.as_bytes());
        all.push(0);
        all.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        all.extend_from_slice(bytes);
    };
    add(PROJECT_FILE, &std::fs::read(dir.join(PROJECT_FILE)).with_context(|| format!("reading {PROJECT_FILE}"))?);
    add(CONFIG_FILE, &std::fs::read(dir.join(CONFIG_FILE)).unwrap_or_default());
    // With each module's web build, when there is one.
    let mut modules: Vec<String> = modules
        .iter()
        .flat_map(|m| [m.clone(), crate::web::web_module(dir, m)])
        .collect();
    modules.sort();
    modules.dedup();
    for m in &modules {
        add(m, &std::fs::read(dir.join(m)).with_context(|| format!("reading {m}"))?);
    }
    Ok(sha256(&all))
}

impl BuildManifest {
    pub fn to_json(&self) -> String {
        json::write(&obj(vec![
            ("format", json::s(FORMAT)),
            ("version", json::n(VERSION)),
            ("inputs", json::s(&self.inputs)),
            (
                "images",
                Json::Arr(
                    self.images
                        .iter()
                        .map(|i| {
                            obj(vec![
                                ("kind", json::s(&i.kind)),
                                ("identity", json::s(&i.identity)),
                                ("base", json::s(format!("{:#x}", i.base))),
                                ("module", json::s(&i.module)),
                                ("object", json::s(&i.object)),
                                ("path", json::s(&i.path)),
                                ("blocks", json::n(i.blocks)),
                                ("bytes", json::n(i.bytes)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "replacements",
                Json::Arr(
                    self.replacements
                        .iter()
                        .map(|r| {
                            obj(vec![
                                ("id", json::n(u64::from(r.id))),
                                ("rule", json::n(u64::from(r.rule))),
                                ("kind", json::s(&r.kind)),
                                ("missing", json::b(r.missing)),
                                ("module", json::s(&r.module)),
                                ("export", json::s(&r.export)),
                                ("abi", json::s(r.abi.name())),
                                ("object", json::s(&r.object)),
                                ("symbol", json::s(&r.symbol)),
                                ("addr", json::s(format!("{:#x}", r.addr))),
                                ("sig", json::s(&r.sig)),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "pins",
                Json::Arr(
                    self.pins
                        .iter()
                        .map(|(p, b)| obj(vec![("path", json::s(p)), ("base", json::s(format!("{b:#x}")))]))
                        .collect(),
                ),
            ),
        ]))
    }

    pub fn from_json(text: &str) -> Result<BuildManifest> {
        let what = BUILD_FILE;
        let v = Json::parse(text).map_err(|e| anyhow!("{what}: {e}"))?;
        if str_field(&v, "format", what)? != FORMAT || field(&v, "version", what)?.as_u64() != Some(VERSION) {
            bail!("{what}: written by another gexc; run ./build.sh");
        }
        let arr = |k: &str| -> Result<Vec<Json>> {
            Ok(field(&v, k, what)?
                .as_arr()
                .ok_or_else(|| anyhow!("{what}: \"{k}\" is not a list"))?
                .to_vec())
        };
        let num = |x: &Json, k: &str| -> Result<u64> {
            field(x, k, what)?.as_u64().ok_or_else(|| anyhow!("{what}: \"{k}\" is not a number"))
        };
        let hex = |x: &Json, k: &str| -> Result<u64> {
            let v = str_field(x, k, what)?;
            u64::from_str_radix(v.trim_start_matches("0x"), 16).map_err(|_| anyhow!("{what}: bad {k} {v}"))
        };
        let mut images = Vec::new();
        for i in arr("images")? {
            images.push(BuiltImage {
                kind: str_field(&i, "kind", what)?,
                identity: str_field(&i, "identity", what)?,
                base: hex(&i, "base")?,
                module: str_field(&i, "module", what)?,
                object: str_field(&i, "object", what)?,
                path: str_field(&i, "path", what)?,
                blocks: num(&i, "blocks")?,
                bytes: num(&i, "bytes")?,
            });
        }
        let mut replacements = Vec::new();
        for r in arr("replacements")? {
            replacements.push(BuiltReplacement {
                id: num(&r, "id")? as u32,
                rule: num(&r, "rule")? as u32,
                kind: str_field(&r, "kind", what)?,
                missing: field(&r, "missing", what)?.as_bool().unwrap_or(false),
                module: str_field(&r, "module", what)?,
                export: str_field(&r, "export", what)?,
                abi: Abi::parse(&str_field(&r, "abi", what)?).ok_or_else(|| anyhow!("{what}: bad abi"))?,
                object: str_field(&r, "object", what)?,
                symbol: str_field(&r, "symbol", what)?,
                addr: hex(&r, "addr")?,
                sig: str_field(&r, "sig", what)?,
            });
        }
        let mut pins = Vec::new();
        for p in arr("pins")? {
            pins.push((str_field(&p, "path", what)?, hex(&p, "base")?));
        }
        Ok(BuildManifest {
            inputs: str_field(&v, "inputs", what)?,
            images,
            replacements,
            pins,
        })
    }

    pub fn load(dir: &Path) -> Result<BuildManifest> {
        let p = dir.join(BUILD_FILE);
        let text = std::fs::read_to_string(&p)
            .with_context(|| format!("reading {} (build the project first: ./build.sh)", p.display()))?;
        BuildManifest::from_json(&text)
    }
}

#[derive(Debug, Default)]
pub struct BuildReport {
    /// The web build: its modules' size, or why there is none. `None` when
    /// the project has no browser runtime.
    pub web: Option<std::result::Result<u64, String>>,
    pub images: Vec<BuiltImage>,
    pub replacements: Vec<BuiltReplacement>,
    pub illegal_blocks: usize,
    pub rootfs_files: usize,
}

/// Check the project's `rootfs/` is what the eject wrote.
fn verify_rootfs(dir: &Path, project: &Project) -> Result<()> {
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
fn find_object<'a>(placed: &'a [Placed], name: &str) -> Result<&'a Placed> {
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
struct Placed {
    obj: Object,
    image: gex_ld::Image,
}

/// Place every object as the run will: executables at `PIE_BASE` (gex's
/// loader), the interpreter at `INTERP_BASE` (likewise), and libraries up
/// from `MMAP_BOTTOM` in the session's order, each at a base the run pins
/// with `RunOptions::pins`. The same rule as `gexc build --closure`.
fn layout(dir: &Path, project: &Project) -> Result<Vec<Placed>> {
    const SLACK: u64 = 0x10000;
    let mut next = gex_abi::MMAP_BOTTOM;
    let mut out = Vec::new();
    for o in &project.objects {
        let p = dir.join(rootfs_rel(&o.path)?);
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        let load = |base: u64| gex_ld::elf::load(&bytes, base).map_err(|e| anyhow!("loading {}: {e:?}", o.path));
        let image = match o.role.as_str() {
            "exe" => load(gex_abi::PIE_BASE)?,
            "interp" => load(gex_abi::INTERP_BASE)?,
            "library" => {
                // Measured by loading it once at the bottom (not at zero,
                // which gex-ld refuses as the runtime's region).
                let span = load(gex_abi::MMAP_BOTTOM)?.brk - gex_abi::MMAP_BOTTOM;
                let base = (next + SLACK - 1) & !(SLACK - 1);
                next = base + ((span + SLACK - 1) & !(SLACK - 1)) + SLACK;
                if next > gex_abi::PLACE_TOP {
                    bail!("the closure does not fit below {:#x}", gex_abi::PLACE_TOP);
                }
                load(base)?
            }
            r => bail!("{}: unknown object role {r:?}", o.path),
        };
        out.push(Placed { obj: o.clone(), image });
    }
    Ok(out)
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
fn symbol_in(p: &Placed, symbol: &str) -> Result<(u64, u64)> {
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
        // The object(s), each checked against the hash it was written for.
        let objs: Vec<&Placed> = if object.is_empty() {
            if want.is_some() {
                bail!("{what}: a \"sha256\" needs the \"object\" it is the hash of");
            }
            placed.iter().collect()
        } else {
            let p = find_object(placed, object).with_context(|| what.clone())?;
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
    let placed = layout(dir, &project)?;
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

fn copy_tree(from: &Path, to: &Path, count: &mut usize) -> Result<()> {
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
    fn translate(&self, out: &str, web: bool) -> Result<(Vec<BuiltImage>, Vec<(String, u64)>, usize)> {
        // The program's closure is numbered as one: its image owns the memory and
        // tables, and every other object's slots continue where the last one's
        // stopped. Other executables (programs the session exec'd) are images of
        // their own.
        let main = self.placed
            .iter()
            .position(|p| p.obj.path == self.project.program.exe && p.obj.role == "exe")
            .ok_or_else(|| anyhow!("the project's program {} is not among its objects", self.project.program.exe))?;
        let mut order: Vec<usize> = vec![main];
        order.extend((0..self.placed.len()).filter(|&i| i != main && self.placed[i].obj.role != "exe"));
        let others: Vec<usize> = (0..self.placed.len()).filter(|&i| i != main && self.placed[i].obj.role == "exe").collect();
        let main_identity = gex_native::aot::image_identity(&self.placed[main].image);

        let mut images = Vec::new();
        let mut illegal = 0;
        let mut pins = Vec::new();
        let mut first_slot = 0u64;
        for (n, &i) in order.iter().chain(others.iter()).enumerate() {
            let p = &self.placed[i];
            let o = &p.obj;
            let in_closure = n < order.len();
            // The browser runs a dynamic program's closure the way gex's own
            // pages do (`gexc build --closure --shared-image`): every object
            // is a demand-filled library module, including the executable, and
            // the process starts from a seed module. An image module writing
            // its slots at instantiation does not mix with the pages the shim
            // numbers for itself.
            let shared_image = web && order.len() > 1;
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
        Ok((images, pins, illegal))
    }
}

pub fn build(dir: &Path) -> Result<BuildReport> {
    let project = Project::load(dir)?;
    if project.isa != gex_abi::ARCH_NAME {
        bail!("the project is for {} guests and this gexc runs {} guests", project.isa, gex_abi::ARCH_NAME);
    }
    let (config, _) = Config::load(dir)?;
    verify_rootfs(dir, &project)?;
    // What the session ran, per object, to seed discovery with.
    let profile = match std::fs::read_to_string(dir.join("session/profile.json")) {
        Ok(t) => Some(gex_xlate::profile::Profile::from_json(&t).map_err(|e| anyhow!("session/profile.json: {e}"))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow!("reading session/profile.json: {e}")),
    };

    let placed = layout(dir, &project)?;
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
    for p in &placed {
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

    let mut report = BuildReport::default();
    let t = Translation { dir, project: &project, placed: &placed, profile: profile.as_ref(), config: &config, intrinsics: &intrinsics };
    let (images, pins, illegal) = t.translate("build/modules", false)?;
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
                let (images, _, _) = t.translate("build/web", true)?;
                let spans: Vec<(String, u64, u64)> = placed
                    .iter()
                    .filter(|p| p.obj.role == "library")
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

/// Where the build put the guest's filesystem.
pub fn rootfs(dir: &Path) -> PathBuf {
    dir.join("build/rootfs")
}
