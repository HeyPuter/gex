//! `build/build.json`: what a project's build made, which a run reads. Apart
//! from the build itself, so a runtime that cannot build (gex-rt, without
//! the translator) still reads it.

use crate::config::CONFIG_FILE;
use crate::json::{self, field, obj, str_field, Json};
use crate::project::PROJECT_FILE;
use anyhow::{anyhow, bail, Context, Result};
use gex_native::aot::Abi;
use std::path::{Path, PathBuf};

fn sha256(bytes: &[u8]) -> String {
    let mut h = gex_cache::sha256::Sha256::new();
    h.update(bytes);
    format!("sha256:{}", gex_cache::sha256::hex(&h.finish()))
}

/// A replacement module's web build, when there is one.
fn web_module(dir: &Path, module: &str) -> String {
    let web = module.strip_suffix(".wasm").map(|m| format!("{m}.web.wasm"));
    match web {
        Some(w) if dir.join(&w).exists() => w,
        _ => module.to_string(),
    }
}

/// Where the build put the guest's filesystem.
pub fn rootfs(dir: &Path) -> PathBuf {
    dir.join("build/rootfs")
}

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
        .flat_map(|m| [m.clone(), web_module(dir, m)])
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
