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

use crate::config::{Config, Coverage, Replace, CONFIG_FILE};
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

/// One translated program image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltImage {
    /// `gex_native::aot::image_identity` of the loaded image.
    pub identity: String,
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
    let mut modules = modules.to_vec();
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
                                ("identity", json::s(&i.identity)),
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
        let mut images = Vec::new();
        for i in arr("images")? {
            images.push(BuiltImage {
                identity: str_field(&i, "identity", what)?,
                module: str_field(&i, "module", what)?,
                object: str_field(&i, "object", what)?,
                path: str_field(&i, "path", what)?,
                blocks: num(&i, "blocks")?,
                bytes: num(&i, "bytes")?,
            });
        }
        let mut replacements = Vec::new();
        for r in arr("replacements")? {
            let addr = str_field(&r, "addr", what)?;
            replacements.push(BuiltReplacement {
                id: num(&r, "id")? as u32,
                module: str_field(&r, "module", what)?,
                export: str_field(&r, "export", what)?,
                abi: Abi::parse(&str_field(&r, "abi", what)?).ok_or_else(|| anyhow!("{what}: bad abi"))?,
                object: str_field(&r, "object", what)?,
                symbol: str_field(&r, "symbol", what)?,
                addr: u64::from_str_radix(addr.trim_start_matches("0x"), 16)
                    .map_err(|_| anyhow!("{what}: bad addr {addr}"))?,
                sig: str_field(&r, "sig", what)?,
            });
        }
        Ok(BuildManifest {
            inputs: str_field(&v, "inputs", what)?,
            images,
            replacements,
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
fn find_object<'a>(project: &'a Project, name: &str) -> Result<&'a Object> {
    if let Some(o) = project.objects.iter().find(|o| o.path == name) {
        return Ok(o);
    }
    let by_name: Vec<&Object> = project
        .objects
        .iter()
        .filter(|o| o.path.rsplit('/').next() == Some(name))
        .collect();
    match by_name.as_slice() {
        [o] => Ok(o),
        [] => bail!(
            "no object named {name:?} (the project's objects: {})",
            project.objects.iter().map(|o| o.path.as_str()).collect::<Vec<_>>().join(", ")
        ),
        _ => bail!("{name:?} names several objects; give its guest path"),
    }
}

/// Load an object as the runtime will: an executable at `PIE_BASE`.
fn load_image(dir: &Path, o: &Object) -> Result<gex_ld::Image> {
    let p = dir.join(rootfs_rel(&o.path)?);
    let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
    gex_ld::elf::load(&bytes, gex_abi::PIE_BASE).map_err(|e| anyhow!("loading {}: {e:?}", o.path))
}

/// Resolve and check every replacement. Returns them numbered, and the
/// replacement blocks per object id.
fn bind_replacements(
    dir: &Path,
    project: &Project,
    config: &Config,
) -> Result<(Vec<BuiltReplacement>, HashMap<String, HashMap<u64, Intrinsic>>)> {
    let mut out = Vec::new();
    let mut per_object: HashMap<String, HashMap<u64, Intrinsic>> = HashMap::new();
    let mut checked: HashMap<(String, String, Abi), String> = HashMap::new();
    for (i, r) in config.replace.iter().enumerate() {
        let Replace { object, sha256: want, symbol, wasm, export, abi } = r;
        let what = format!("{CONFIG_FILE}: replace[{i}] ({symbol} in {object})");
        let o = find_object(project, object).with_context(|| what.clone())?;
        if &o.id != want {
            bail!(
                "{what} is stale: it was written for {want}, and the project's {} is now {}. \
                 Check the replacement still fits and update its \"sha256\"",
                o.path,
                o.id
            );
        }
        if o.role != "exe" {
            bail!("{what}: {} is a {}; eject v0 replaces functions in executables only", o.path, o.role);
        }
        let image = load_image(dir, o)?;
        let (tlo, thi) = image.text_range;
        let mut addrs: Vec<u64> = image
            .symbols
            .iter()
            .filter(|s| s.name == *symbol && !s.ifunc)
            .map(|s| s.value.wrapping_add(image.bias))
            .filter(|a| *a >= tlo && *a < thi)
            .collect();
        addrs.sort_unstable();
        addrs.dedup();
        let addr = match addrs.as_slice() {
            [a] => *a,
            [] => bail!("{what}: {} defines no function `{symbol}` (is it stripped?)", o.path),
            _ => bail!("{what}: {} defines `{symbol}` at several addresses", o.path),
        };
        let map = per_object.entry(o.id.clone()).or_default();
        if let Some(Intrinsic::Replace(other)) = map.get(&addr) {
            bail!("{what}: {:#x} is already replaced by replace[{other}]", addr);
        }
        let module_path = dir.join(wasm);
        let bytes = std::fs::read(&module_path).with_context(|| {
            if wasm == crate::config::USER_WASM {
                format!("{what}: no {wasm}; build.sh compiles user/src/*.c into it")
            } else {
                format!("{what}: reading {wasm}")
            }
        })?;
        let key = (wasm.clone(), export.clone(), *abi);
        let sig = match checked.get(&key) {
            Some(s) => s.clone(),
            None => {
                let s = gex_native::aot::check_replacement(&bytes, export, *abi)
                    .with_context(|| format!("{what}: {wasm}"))?;
                checked.insert(key, s.clone());
                s
            }
        };
        let id = i as u32;
        map.insert(addr, Intrinsic::Replace(id));
        out.push(BuiltReplacement {
            id,
            module: wasm.clone(),
            export: export.clone(),
            abi: *abi,
            object: o.id.clone(),
            symbol: symbol.clone(),
            addr,
            sig,
        });
    }
    Ok((out, per_object))
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

pub fn build(dir: &Path) -> Result<BuildReport> {
    let project = Project::load(dir)?;
    if project.isa != gex_abi::ARCH_NAME {
        bail!("the project is for {} guests and this gexc runs {} guests", project.isa, gex_abi::ARCH_NAME);
    }
    let (config, _) = Config::load(dir)?;
    verify_rootfs(dir, &project)?;
    if let Some(o) = project.objects.iter().find(|o| o.role != "exe") {
        bail!(
            "{} is dynamically linked ({} is its {}): eject v0 builds statically linked \
             programs only",
            project.program.exe,
            o.path,
            o.role
        );
    }

    // What the session ran, per object, to seed discovery with.
    let profile = match std::fs::read_to_string(dir.join("session/profile.json")) {
        Ok(t) => Some(gex_xlate::profile::Profile::from_json(&t).map_err(|e| anyhow!("session/profile.json: {e}"))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(anyhow!("reading session/profile.json: {e}")),
    };

    let (replacements, mut replaced) = bind_replacements(dir, &project, &config)?;

    let out = dir.join("build");
    let modules_dir = out.join("modules");
    for d in [&modules_dir, &out.join("rootfs")] {
        if d.exists() {
            std::fs::remove_dir_all(d).with_context(|| format!("removing {}", d.display()))?;
        }
    }
    let _ = std::fs::remove_file(dir.join(BUILD_FILE));
    std::fs::create_dir_all(&modules_dir).with_context(|| format!("creating {}", modules_dir.display()))?;

    let mut report = BuildReport::default();
    for o in project.objects.iter().filter(|o| o.role == "exe") {
        let image = load_image(dir, o)?;
        gex_xlate::set_compat(image.class32);
        // gex's own library intrinsics, as a closure build and the runtime
        // translator use them, then the user's replacements over them.
        let (tlo, thi) = image.text_range;
        let mut intrinsics: HashMap<u64, Intrinsic> = HashMap::new();
        for s in &image.symbols {
            let a = s.value.wrapping_add(image.bias);
            if s.ifunc || a < tlo || a >= thi {
                continue;
            }
            if let Some(k) = gex_xlate::intrinsic::recognise(&s.name) {
                intrinsics.insert(a, k);
            }
        }
        intrinsics.extend(replaced.remove(&o.id).unwrap_or_default());
        let seeds: Vec<u64> = profile
            .as_ref()
            .and_then(|p| p.objects.get(&o.id))
            .map(|op| {
                op.blocks
                    .keys()
                    .map(|off| off.wrapping_add(image.bias))
                    .filter(|a| *a >= tlo && *a < thi)
                    .collect()
            })
            .unwrap_or_default();
        let opts = Options {
            exhaustive: config.coverage == Coverage::Exhaustive,
            extra_entries: seeds,
            complete_pages: config.coverage == Coverage::Profile,
            intrinsics,
            shared_memory: true,
            ..Options::default()
        };
        let (module, stats) = gex_xlate::translate_image(&image, &opts);
        gex_native::validate_module(&module.wasm).with_context(|| format!("the translation of {} is not valid wasm", o.path))?;
        let name = o.path.rsplit('/').next().unwrap_or("image");
        let hex = o.id.trim_start_matches("sha256:");
        let rel = format!("build/modules/{name}-{}.wasm", &hex[..12.min(hex.len())]);
        std::fs::write(dir.join(&rel), &module.wasm).with_context(|| format!("writing {rel}"))?;
        report.illegal_blocks += stats.illegal_blocks;
        report.images.push(BuiltImage {
            identity: gex_native::aot::image_identity(&image),
            module: rel,
            object: o.id.clone(),
            path: o.path.clone(),
            blocks: stats.blocks as u64,
            bytes: module.wasm.len() as u64,
        });
    }
    if !replaced.is_empty() {
        bail!("internal: replacements bound to objects that were not built");
    }

    // The guest's filesystem: what the session read, then the user's files.
    let mut n = 0;
    copy_tree(&dir.join("rootfs"), &out.join("rootfs"), &mut n)?;
    let user_files = dir.join("user/files");
    if user_files.is_dir() {
        copy_tree(&user_files, &out.join("rootfs"), &mut n)?;
    }
    report.rootfs_files = n;

    let modules: Vec<String> = replacements.iter().map(|r| r.module.clone()).collect();
    let manifest = BuildManifest {
        inputs: inputs_id(dir, &modules)?,
        images: report.images.clone(),
        replacements: replacements.clone(),
    };
    std::fs::write(dir.join(BUILD_FILE), manifest.to_json()).with_context(|| format!("writing {BUILD_FILE}"))?;
    report.replacements = replacements;
    Ok(report)
}

/// Where the build put the guest's filesystem.
pub fn rootfs(dir: &Path) -> PathBuf {
    dir.join("build/rootfs")
}
