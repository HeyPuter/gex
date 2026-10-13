//! Windows programs (docs/eject-pe.md): a project ejected from a session of
//! a PE program on gex's NT layer builds every image the session mapped as
//! a whole-image function-tier unit at its recorded base, pins those bases
//! with an exact layout, and runs with no run-time translation at all
//! (`gex_native::aot_only`).
//!
//! The session's `nt` section says how the NT layer was set up; the rootfs
//! holds the images and the files the NT layer opened, at their guest paths.

#[cfg(feature = "build")]
use crate::build::{copy_tree, verify_rootfs};
use crate::manifest::{BuildManifest, BuiltImage, BUILD_FILE};
use crate::config::Config;
use crate::project::{rootfs_rel, Project};
use anyhow::{anyhow, bail, Context, Result};
use gex_kernel::nt;
use gex_native::nt_cgen;
use gex_native::{Exit, RunOptions};
use gex_xlate::profile::{NtSession, Session};
use std::path::{Path, PathBuf};

#[cfg(feature = "build")]
/// What a provider includes (gex-cgen's), for `gen/provider.h`.
pub const PROVIDER_H: &str = include_str!("../../gex-cgen/providers/provider.h");

#[cfg(feature = "build")]
/// Whether the PE file at `path` is LuaJIT's lua51.dll (it exports
/// `luaJIT_setmode`): its trace compiler writes x86 at run time, which an
/// ejected project cannot run, so it gets the Lua provider.
pub fn is_luajit(path: &Path) -> Result<bool> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    Ok(pe.exports.entries.iter().any(|e| e.names.iter().any(|n| n == "luaJIT_setmode")))
}

/// Where a configured provider's module goes.
fn provider_wasm(dir: &Path, p: &crate::config::Provider) -> PathBuf {
    dir.join("build/providers").join(p.library.to_ascii_lowercase().replace(".dll", ".wasm"))
}

#[cfg(feature = "build")]
/// The imports a stateful provider may have.
fn check_provider_imports(wasm: &[u8], what: &str) -> Result<()> {
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::ImportSection(r) = payload? {
            for i in r {
                let i = i?;
                let ok = match i.module {
                    "env" => matches!(i.name, "memory" | "__indirect_function_table" | "__stack_pointer" | "__memory_base" | "__table_base"),
                    "gex" => i.name.starts_with("provider_"),
                    _ => false,
                };
                if !ok {
                    bail!("{what} imports {}.{}, which a provider cannot have (give it a definition)", i.module, i.name);
                }
            }
        }
    }
    Ok(())
}

/// The project's stateful providers, as the runtime takes them.
fn lib_providers(dir: &Path, config: &Config) -> Vec<gex_native::libprov::LibProvider> {
    config
        .providers
        .iter()
        .map(|p| gex_native::libprov::LibProvider {
            name: p.library.clone(),
            dlls: vec![p.library.to_ascii_lowercase()],
            wasm: provider_wasm(dir, p),
        })
        .collect()
}

/// The recorded session of a project, when it was a Windows program's.
pub fn nt_session(dir: &Path) -> Result<Option<NtSession>> {
    let p = dir.join("session/session.json");
    let text = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    Ok(Session::from_json(&text).map_err(|e| anyhow!("{}: {e}", p.display()))?.nt)
}

/// Where the project's units are: the runtime's `GEX_CGEN_CACHE`.
pub fn units_dir(dir: &Path) -> PathBuf {
    dir.join("build/units")
}

fn abi_of(nt: &NtSession) -> Result<nt::Abi> {
    match nt.abi.as_str() {
        "p32" => Ok(nt::Abi::P32),
        "p64" => Ok(nt::Abi::P64),
        a => bail!("session.json: unknown NT ABI {a:?}"),
    }
}

/// The images to build: each (guest path, base) once, in mapping order.
fn images(nt: &NtSession) -> Vec<&gex_xlate::profile::NtImage> {
    let mut v: Vec<&gex_xlate::profile::NtImage> = Vec::new();
    for m in &nt.images {
        if !v.iter().any(|x| x.path == m.path && x.base == m.base) {
            v.push(m);
        }
    }
    v
}

/// What a PE build made.
#[derive(Debug, Default)]
pub struct PeReport {
    pub units: Vec<(String, u64, usize, u64, &'static str)>,
    pub providers: Vec<String>,
    /// Region entries built, by image.
    pub regions: Vec<(String, usize)>,
    /// Entries the session reached by a call that no unit has even after
    /// recovery was seeded with them, by image: (image, count, first few).
    pub uncovered: Vec<(String, usize, Vec<u64>)>,
    pub rootfs_files: usize,
    /// The web build's units (browser flavour), by image: (image, how), and
    /// region entries made for it; empty without a browser runtime.
    pub web_units: Vec<(String, &'static str)>,
    /// Functions units were built without (left to tier-1: clang wanted a
    /// shadow stack), by image.
    pub left: Vec<(String, Vec<u64>)>,
    /// Link groups built: (members, IAT call sites made direct, bytes).
    pub groups: Vec<(String, usize, u64)>,
    pub web_regions: usize,
}

/// Where a Windows program's browser runtime is vendored (`gexc eject
/// --web-shim`): the x86 shared-memory shim, its kernel and seed, and the
/// host JavaScript (`js/`).
pub const WEB_NT_DIR: &str = "runtime/web/nt";
/// The files of [`WEB_NT_DIR`] besides `js/`.
pub const WEB_NT_WASM: [&str; 3] = ["gex_browser.x86.shared.wasm", "gex_kernel.x86.wasm", "seed.x86.shared.wasm"];
/// The host JavaScript a Windows program's web run needs (shims/browser/js).
pub const WEB_NT_JS: [(&str, &str); 11] = [
    ("run-node.mjs", include_str!("../../../shims/browser/js/run-node.mjs")),
    ("host.mjs", include_str!("../../../shims/browser/js/host.mjs")),
    ("worker.mjs", include_str!("../../../shims/browser/js/worker.mjs")),
    ("gex.mjs", include_str!("../../../shims/browser/js/gex.mjs")),
    ("gl-bridge.mjs", include_str!("../../../shims/browser/js/gl-bridge.mjs")),
    ("wl-bridge.mjs", include_str!("../../../shims/browser/js/wl-bridge.mjs")),
    ("net-node.mjs", include_str!("../../../shims/browser/js/net-node.mjs")),
    ("net-bridge.mjs", include_str!("../../../shims/browser/js/net-bridge.mjs")),
    ("lazy-fetch.mjs", include_str!("../../../shims/browser/js/lazy-fetch.mjs")),
    ("single.mjs", include_str!("../../../shims/browser/js/single.mjs")),
    ("xcache.mjs", include_str!("../../../shims/browser/js/xcache.mjs")),
];
/// `run.mjs` of a Windows program's project: the web build under Node, on
/// the browser runtime a page loads (run-node.mjs), AOT-only.
pub const WEB_NT_RUN_MJS: &str = r#"#!/usr/bin/env node
// Generated by `gexc eject`: run this Windows program's web build under Node,
// on the browser runtime a page loads (runtime/web/nt), with no run-time
// translation: the units in build/web/cgen are all the code there is.
//
//   ./run.mjs                  the recorded arguments
//   ./run.mjs ARGS...          these instead
//   ./run.mjs --jit-fallback [-- ARGS...]   translate what the build never saw
//   ./run.mjs --record-entries [-- ARGS...] a training run: with the fallback,
//        writing where the browser entered code the units have no entry for
//        to user/web-entries.txt, which ./build.sh makes region entries of
//
// GEX_NT_* variables set now are passed to the NT layer (GEX_NT_STATS, ...).
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const web = join(here, 'runtime/web/nt');
const runFile = join(here, 'build/web/nt.json');
if (!existsSync(join(web, 'gex_browser.x86.shared.wasm'))) {
  console.error('gex-eject: no browser runtime in runtime/web/nt (eject with --web-shim web/public/gex)');
  process.exit(2);
}
if (!existsSync(runFile)) {
  console.error('gex-eject: no web build (run ./build.sh)');
  process.exit(2);
}
const m = JSON.parse(readFileSync(runFile, 'utf8'));
let av = process.argv.slice(2);
let jit = false;
let record = false;
if (av[0] === '--jit-fallback') { jit = true; av = av.slice(1); }
if (av[0] === '--record-entries') { jit = record = true; av = av.slice(1); }
if (av[0] === '--') av = av.slice(1);
const args = av.length ? av : m.args;
const rootfs = join(here, 'build/rootfs');
const env = [`GEX_WINE_PREFIX=${m.prefix}`, ...m.settings, ...m.env];
for (const [k, v] of Object.entries(process.env)) if (k.startsWith('GEX_NT_')) env.push(`${k}=${v}`);
if (record) {
  env.push('GEX_NT_ENTRY_LOG=1');
  const file = join(here, 'user/web-entries.txt');
  const seen = new Set(existsSync(file) ? readFileSync(file, 'utf8').split('\n').filter(Boolean) : []);
  const before = seen.size;
  // The Workers' output reaches this process's stderr (Node pipes it
  // there): taken out line by line.
  const write = process.stderr.write.bind(process.stderr);
  let partial = '';
  process.stderr.write = (chunk, ...rest) => {
    const lines = (partial + (typeof chunk === 'string' ? chunk : Buffer.from(chunk).toString())).split('\n');
    partial = lines.pop();
    const keep = [];
    for (const l of lines) {
      const m = l.match(/gex-nt: web entry (0x[0-9a-f]+)/);
      if (m) seen.add(m[1]);
      else keep.push(l + '\n');
    }
    return keep.length ? write(keep.join(''), ...rest) : true;
  };
  process.on('exit', () => {
    writeFileSync(file, [...seen].sort().join('\n') + '\n');
    write(`gex-eject: ${seen.size - before} new web entries (${seen.size} in all) in user/web-entries.txt; run ./build.sh\n`);
  });
}
process.env.GEX_ARCH = 'x86';
process.env.GEX_SHARED = '1';
process.env.GEX_WEB_DIR = web;
process.env.GEX_CGEN_CACHE = join(here, m.cgen ?? 'build/web/cgen');
if (m.unguarded) process.env.GEX_CGEN_UNGUARDED = '1';
process.argv = [
  process.argv[0], join(web, 'js/run-node.mjs'),
  '--cgen', ...(jit ? (record ? ['--aot'] : []) : ['--no-jit']),
  '--dir', `/=${rootfs}`, '--argv0', m.exe, '--cwd', m.cwd,
  ...env.flatMap((e) => ['--env', e]),
  join(web, 'seed.x86.shared.wasm'), join(rootfs, m.exe), ...args,
];
await import(pathToFileURL(join(web, 'js/run-node.mjs')));
"#;

/// Where a Windows program's page is vendored (`gexc eject --web-page`).
pub const WEB_NT_PAGE: &str = "runtime/web/page";

/// `serve.mjs` of a Windows program's project: the user's page
/// (user/index.html) at `/`, the app library (runtime/web/page: app.mjs, its
/// chunks, the runtime's wasm) beside it, this project under `/project/`,
/// cross-origin isolated: the layout of the static site eject-site.mjs makes.
pub const WEB_NT_SERVE_MJS: &str = r#"#!/usr/bin/env node
// Serve this ejected project's page (user/index.html, which calls app.mjs's
// start()) with the headers gex's runtime needs (cross-origin isolation, for
// SharedArrayBuffer). Generated by `gexc eject`. A training run of the web
// build is a page whose start() asks for it:
//   start({ container, translation: true, env: { GEX_NT_ENTRY_LOG: 1 }, debug: { aot: 'record' } })
// then keep the "gex-nt: web entry" lines in user/web-entries.txt.
//   node serve.mjs [port]      then open http://localhost:PORT/
import { createServer } from 'node:http';
import { createReadStream, statSync } from 'node:fs';
import { dirname, extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const page = join(here, 'runtime/web/page');
const port = Number(process.argv[2] ?? 8000);
const types = { '.html': 'text/html', '.mjs': 'text/javascript', '.js': 'text/javascript', '.css': 'text/css',
  '.json': 'application/json', '.wasm': 'application/wasm', '.tar': 'application/x-tar' };
createServer((req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname));
  if (path.includes('..')) { res.writeHead(403).end(); return; }
  const file = path === '/' || path === '/index.html' ? join(here, 'user/index.html')
    : path.startsWith('/project/') ? join(here, path.slice('/project/'.length))
    : join(page, path);
  let st;
  try { st = statSync(file); } catch { st = null; }
  if (!st?.isFile()) { res.writeHead(404).end(); return; }
  res.writeHead(200, {
    'Content-Type': types[extname(file)] ?? 'application/octet-stream',
    'Content-Length': st.size,
    'Cross-Origin-Opener-Policy': 'same-origin',
    'Cross-Origin-Embedder-Policy': 'require-corp',
    'Cache-Control': 'no-cache',
  });
  createReadStream(file).pipe(res);
}).listen(port, '127.0.0.1', () => console.log(`http://localhost:${port}/`));
"#;

/// What the web launcher reads: how to start the program (build/web/nt.json).
pub const WEB_NT_RUN: &str = "build/web/nt.json";

#[cfg(feature = "build")]
/// Link `from` to `to` (a copy across file systems).
fn link_or_copy(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        return Ok(());
    }
    if std::fs::hard_link(from, to).is_err() {
        std::fs::copy(from, to).with_context(|| format!("copying {}", from.display()))?;
    }
    Ok(())
}

#[cfg(feature = "build")]
/// Take a unit already built elsewhere (`GEX_EJECT_UNIT_CACHE`: units are
/// keyed by content, so one with the same key is the same unit).
fn reuse(key_dir: &Path, sources: &[PathBuf]) -> Result<bool> {
    let key = key_dir.file_name().unwrap();
    for s in sources {
        let src = s.join(key);
        if src.join("c.wasm").exists() && src.join("c.entries").exists() {
            std::fs::create_dir_all(key_dir)?;
            for f in ["c.entries", "c.cwasm", "c.wasm"] {
                if src.join(f).exists() {
                    link_or_copy(&src.join(f), &key_dir.join(f))?;
                }
            }
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(feature = "build")]
pub fn build(dir: &Path, project: &Project, config: &Config) -> Result<PeReport> {
    let ntss = nt_session(dir)?.ok_or_else(|| anyhow!("not a Windows program's project"))?;
    verify_rootfs(dir, project)?;
    let mut report = PeReport::default();

    // The guest's filesystem, then the exact layout in its prefix.
    let out = dir.join("build");
    let root = out.join("rootfs");
    if root.exists() {
        std::fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
    }
    let _ = std::fs::remove_file(dir.join(BUILD_FILE));
    let mut n = 0;
    copy_tree(&dir.join("rootfs"), &root, &mut n)?;
    let user_files = dir.join("user/files");
    if user_files.is_dir() {
        copy_tree(&user_files, &root, &mut n)?;
    }
    report.rootfs_files = n;
    let prefix_host = root.join(rootfs_rel(&ntss.prefix)?.strip_prefix("rootfs")?);
    let wine = std::fs::read_to_string(prefix_host.join(".wine-version"))
        .with_context(|| format!("{}/.wine-version (the recorded prefix's)", ntss.prefix))?;
    let mut layout = format!("{}\nwine {}\nexact\n", nt::section::LAYOUT_VERSION, wine.trim());
    for m in images(&ntss) {
        let rel = m.path.strip_prefix(&format!("{}/", ntss.prefix)).unwrap_or(&m.path);
        layout.push_str(&format!("{:#x} {:#x} {} {}\n", m.base, m.size, m.id, rel));
    }
    std::fs::write(prefix_host.join(nt::section::LAYOUT_FILE), layout)?;

    // Units, into the project's own cache.
    let units = units_dir(dir);
    std::fs::create_dir_all(&units)?;
    std::env::set_var("GEX_CGEN_CACHE", &units);
    // What a runtime without a compiler (gex-rt) needs besides the units:
    // every module an AOT-only process makes at run time, compiled here.
    let pre = units.join("precompiled");
    std::env::set_var("GEX_PRECOMPILED", &pre);
    gex_native::aot_only::precompile(&pre)?;
    let sources: Vec<PathBuf> = std::env::var("GEX_EJECT_UNIT_CACHE")
        .map(|v| v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default();
    // Indirect-call hints (`<cache>/icall/<image>.txt`, part of a unit's key)
    // from the project (user/icall/) and from the caches units are reused
    // from, so a reused unit built with hints is found by its key.
    std::fs::create_dir_all(units.join("icall"))?;
    for src in std::iter::once(dir.join("user")).chain(sources.iter().cloned()) {
        if let Ok(rd) = std::fs::read_dir(src.join("icall")) {
            for e in rd.flatten() {
                let to = units.join("icall").join(e.file_name());
                if !to.exists() {
                    std::fs::copy(e.path(), &to)?;
                }
            }
        }
    }
    let jobs = std::env::var("JOBS").ok().and_then(|v| v.parse().ok()).unwrap_or(4usize);
    let class32 = abi_of(&ntss)? == nt::Abi::P32;
    if class32 {
        gex_xlate::set_compat(true);
    }
    let engine = gex_native::engine()?;
    let compile = |c_wasm: &Path| -> Result<()> {
        let bytes = std::fs::read(c_wasm)?;
        gex_cache::load_or_compile_at(&engine, &c_wasm.with_extension("cwasm"), &bytes)
            .with_context(|| format!("compiling {}", c_wasm.display()))?;
        Ok(())
    };
    // The NT trampoline page's.
    {
        let (image, _) = gex_kernel::nt::boot::trampoline_input(class32);
        let key = gex_cgen::image::image_key(&image);
        let made = reuse(&units.join(&key), &sources)?;
        let p = nt_cgen::prepare_trampolines(class32, jobs)?;
        compile(&p)?;
        report.units.push(("[nt trampolines]".into(), 0, 0, std::fs::metadata(&p)?.len(), if made { "reused" } else { "built" }));
    }
    let profile = std::fs::read_to_string(dir.join("session/profile.json"))
        .ok()
        .map(|t| gex_xlate::profile::Profile::from_json(&t).map_err(|e| anyhow!("session/profile.json: {e}")))
        .transpose()?;
    let mut images_out = Vec::new();
    let mut web_inputs: Vec<(String, u64, Vec<u8>, Vec<u64>)> = Vec::new();
    let web_entries = web_entries(dir);
    for m in images(&ntss) {
        let host = root.join(rootfs_rel(&m.path)?.strip_prefix("rootfs")?);
        let bytes = std::fs::read(&host).with_context(|| format!("reading {}", host.display()))?;
        let pe = gex_ld::pe::parse(&bytes).map_err(|e| anyhow!("{}: {e}", m.path))?;
        let key_dir = nt_cgen::unit_dir(&pe, m.base)?;
        let bias = m.base.wrapping_sub(m.preferred);
        // Entries the session reached by an indirect call: function starts
        // recovery may not find on its own (a thread start only an
        // immediate names, a callback); seeds of user/eject.json besides.
        // Those the unit lacks become region entries.
        let mut seeds: Vec<u64> = Vec::new();
        for op in profile.iter().flat_map(|pr| pr.objects.values()) {
            for site in op.indirect.values().filter(|s| s.call) {
                seeds.extend(site.targets.keys().filter(|l| l.obj == m.id).map(|l| l.off.wrapping_add(bias)));
            }
        }
        let name = m.path.rsplit('/').next().unwrap_or("").to_ascii_lowercase();
        for s in &config.seeds {
            if let Some((img, off)) = s.split_once('+') {
                if img.eq_ignore_ascii_case(&name) {
                    let off = u64::from_str_radix(off.trim_start_matches("0x"), 16).map_err(|_| anyhow!("user/eject.json: seed {s:?}"))?;
                    seeds.push(m.base + off);
                }
            }
        }
        let how = if key_dir.join("c.wasm").exists() {
            "present"
        } else if reuse(&key_dir, &sources)? {
            "reused"
        } else {
            eprintln!("  building the unit for {} at {:#x} ...", m.path, m.base);
            "built"
        };
        let p = nt_cgen::prepare_pe(&pe, m.base, jobs, true).with_context(|| format!("building the unit for {}", m.path))?;
        compile(&p)?;
        note_left(&pe, m.base, &p, &m.path, "", &mut report)?;
        // Where code ran without a unit although the image had one (a resume
        // into the middle of a function, a unit's leave): region entries,
        // a module of their own beside the unit (docs/eject-pe.md §3.3).
        let end = m.base + m.size;
        let mut interior: Vec<u64> = ntss.entries.iter().copied().filter(|&a| a >= m.base && a < end).collect();
        // And the starts above that recovery did not find: the unit's key
        // and contents stay those of the image alone.
        interior.extend(seeds);
        // And where a training run of the web build entered (its units are
        // these ones, unguarded): one region module for both runtimes.
        interior.extend(web_entries.iter().copied().filter(|&a| a >= m.base && a < end));
        web_inputs.push((m.path.clone(), m.base, bytes.clone(), interior.clone()));
        if !interior.is_empty() {
            let failed = nt_cgen::prepare_regions(&pe, m.base, &p, &interior, jobs)?;
            if let Some(r) = nt_cgen::region_module(&p) {
                compile(&r)?;
                report.regions.push((m.path.clone(), interior.len() - failed.len()));
            }
            if !failed.is_empty() {
                report.uncovered.push((m.path.clone(), failed.len(), failed.iter().take(8).copied().collect()));
            }
        }
        for name in nt_cgen::prepare_providers(&pe)? {
            if !report.providers.contains(&name) {
                report.providers.push(name);
            }
        }
        let entries = gex_cgen::image::read_entries(&p)?;
        report.units.push((m.path.clone(), m.base, entries.len(), std::fs::metadata(&p)?.len(), how));
        images_out.push(BuiltImage {
            kind: "unit".into(),
            identity: key_dir.file_name().unwrap().to_string_lossy().into_owned(),
            base: m.base,
            module: p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().into_owned(),
            object: m.id.clone(),
            path: m.path.clone(),
            blocks: entries.len() as u64,
            bytes: std::fs::metadata(&p)?.len(),
        });
    }
    // Stateful providers, from their sources (Lua: PUC Lua and its binding).
    for p in &config.providers {
        let out = provider_wasm(dir, p);
        std::fs::create_dir_all(out.parent().unwrap())?;
        let script = dir.join(&p.dir).join("build.sh");
        let status = std::process::Command::new("sh")
            .arg(&script)
            .arg(&out)
            .arg(dir.join("gen"))
            .status()
            .with_context(|| format!("running {}", script.display()))?;
        if !status.success() {
            bail!("building the provider for {} ({}) failed", p.library, script.display());
        }
        let wasm = std::fs::read(&out)?;
        check_provider_imports(&wasm, &format!("the provider for {}", p.library))?;
        // Its compiled form, named by its content as the runtime looks it
        // up (`libprov::cwasm_path`): gex-rt has no compiler to make it.
        // Older ones beside it are stale.
        let cwasm = gex_native::libprov::cwasm_path(&out, &wasm);
        if let (Some(d), Some(stem)) = (out.parent(), out.file_stem().and_then(|s| s.to_str())) {
            for e in std::fs::read_dir(d)?.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with(&format!("{stem}.")) && n.ends_with(".cwasm") && e.path() != cwasm {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        gex_cache::load_or_compile_at(&engine, &cwasm, &wasm).with_context(|| format!("compiling the provider for {}", p.library))?;
        report.providers.push(format!("{} ({} KiB, stateful)", p.library, wasm.len() / 1024));
    }
    // Link groups (docs/eject-pe.md §6a): members by file name, one module
    // each, and the index a runtime reads to install them.
    let mut index = String::new();
    for names in &config.link_groups {
        let mut parsed = Vec::new();
        for n in names {
            let m = images(&ntss)
                .into_iter()
                .find(|m| m.path.rsplit('/').next().is_some_and(|f| f.eq_ignore_ascii_case(n)))
                .ok_or_else(|| anyhow!("user/eject.json: link group member {n} is not an image the session mapped"))?;
            let host = root.join(rootfs_rel(&m.path)?.strip_prefix("rootfs")?);
            let bytes = std::fs::read(&host).with_context(|| format!("reading {}", host.display()))?;
            parsed.push((m.path.clone(), m.base, gex_ld::pe::parse(&bytes).map_err(|e| anyhow!("{}: {e}", m.path))?));
        }
        let members: Vec<nt_cgen::GroupMember<'_>> = parsed.iter().map(|(path, base, pe)| nt_cgen::GroupMember { path, pe, base: *base }).collect();
        eprintln!("  building the link group {} ...", names.join(" + "));
        let g = nt_cgen::prepare_group(&members, jobs).with_context(|| format!("building the link group {}", names.join(" + ")))?;
        compile(&g.module)?;
        index.push_str(&format!("{} {}\n", g.key, g.member_keys.join(" ")));
        report.groups.push((names.join(" + "), g.iat_hints, std::fs::metadata(&g.module)?.len()));
    }
    std::fs::create_dir_all(units.join("groups"))?;
    std::fs::write(units.join("groups/index.txt"), &index)?;
    // The web build's units, when the project has a browser runtime.
    if dir.join(WEB_NT_DIR).join(WEB_NT_WASM[0]).exists() {
        let stateful: Vec<(String, PathBuf)> = config.providers.iter().map(|p| (p.library.clone(), provider_wasm(dir, p))).collect();
        build_web(dir, &ntss, project, config.args.as_deref(), &config.start, &units, &web_inputs, &stateful, class32, jobs, &mut report)?;
        std::env::set_var("GEX_CGEN_CACHE", &units);
    }
    let manifest = BuildManifest {
        inputs: crate::manifest::inputs_id(dir, &[])?,
        images: images_out,
        replacements: Vec::new(),
        pins: Vec::new(),
    };
    std::fs::write(dir.join(BUILD_FILE), manifest.to_json()).with_context(|| format!("writing {BUILD_FILE}"))?;
    Ok(report)
}

#[cfg(feature = "build")]
/// Where a training run of the web build (`./run.mjs --record-entries`,
/// a page whose `start()` asks for a training run, see `WEB_NT_SERVE_MJS`)
/// entered code the units have no entry for.
fn web_entries(dir: &Path) -> Vec<u64> {
    std::fs::read_to_string(dir.join("user/web-entries.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| u64::from_str_radix(l.trim().trim_start_matches("0x"), 16).ok())
        .collect()
}

#[cfg(feature = "build")]
/// Functions a unit was built without (gex-cgen left them to tier-1):
/// uncovered in a project that runs with no translation.
fn note_left(pe: &gex_ld::pe::PeImage, base: u64, unit: &Path, path: &str, what: &str, report: &mut PeReport) -> Result<()> {
    let left = nt_cgen::left_out(pe, base, unit)?;
    if !left.is_empty() {
        report.left.push((format!("{path}{what}"), left));
    }
    Ok(())
}

#[cfg(feature = "build")]
/// The web build (docs/eject-pe.md §6): every image's unit in the browser's
/// flavour (guarded: the browser's NT layer keeps host data above a guard
/// limit), its region entries and providers, into `build/web/cgen` (the
/// layout `GEX_CGEN_CACHE` has, which the shim reads), and how to start the
/// program (`build/web/nt.json`). Units are reused from
/// `GEX_EJECT_WEB_UNIT_CACHE` (a browser unit cache) by key. Nothing is
/// compiled here: the browser compiles each module once per page.
#[allow(clippy::too_many_arguments)]
fn build_web(
    dir: &Path,
    ntss: &NtSession,
    project: &Project,
    // user/eject.json's "args", over the recorded ones (as `eject run`).
    args: Option<&[String]>,
    // user/eject.json's "start" (a JSON object's text): start() defaults.
    start: &str,
    native: &Path,
    inputs: &[(String, u64, Vec<u8>, Vec<u64>)],
    stateful: &[(String, PathBuf)],
    class32: bool,
    jobs: usize,
    report: &mut PeReport,
) -> Result<()> {
    let web = dir.join("build/web/cgen");
    let guarded_build = std::env::var("GEX_EJECT_WEB_GUARDED").as_deref() == Ok("1");
    std::fs::create_dir_all(dir.join("build/web"))?;
    if guarded_build {
        std::fs::create_dir_all(&web)?;
    }
    // Recovery and C objects do not depend on the flavour (objects are keyed
    // by their C): the native build's, shared.
    for d in ["recovery", "objects"].into_iter().filter(|_| guarded_build) {
        let to = web.join(d);
        if std::fs::symlink_metadata(&to).is_err() {
            std::fs::create_dir_all(native.join(d))?;
            std::os::unix::fs::symlink(Path::new("../../units").join(d), &to)?;
        }
    }
    if guarded_build {
        std::fs::create_dir_all(web.join("icall"))?;
    }
    if let Ok(rd) = std::fs::read_dir(native.join("icall")).map(|r| r.filter(|_| guarded_build)) {
        for e in rd.flatten() {
            let to = web.join("icall").join(e.file_name());
            if !to.exists() {
                std::fs::copy(e.path(), &to)?;
            }
        }
    }
    let mut sources: Vec<PathBuf> = std::env::var("GEX_EJECT_WEB_UNIT_CACHE")
        .map(|v| v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default();
    let web_entries = web_entries(dir);
    // The unguarded flavour (stream B's step 0): the native units, by the
    // same keys, so the web build adds nothing to compile. The browser's
    // guard protects nothing its memory's end does not (docs/browser-units-
    // plan.md). GEX_EJECT_WEB_GUARDED=1: the guarded flavour, built apart.
    let guarded = std::env::var("GEX_EJECT_WEB_GUARDED").as_deref() == Ok("1");
    let web = if guarded {
        web
    } else {
        let _ = std::fs::remove_dir_all(&web);
        sources.extend(std::env::var("GEX_EJECT_UNIT_CACHE").map(|v| v.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect::<Vec<_>>()).unwrap_or_default());
        native.to_path_buf()
    };
    std::env::set_var("GEX_CGEN_CACHE", &web);
    gex_cgen::image::set_unguarded(!guarded);
    // Each web unit's image key and identity as the browser computes them
    // (sha256 over every byte of every image: ~1 s of the page's start-up),
    // so the browser can take them from here (GEX_NT_IDENTITIES).
    let mut identities: Vec<String> = Vec::new();
    // The NT trampolines' unit, which is no image's (for eject-site to pack).
    let mut tramp_key = String::new();
    let r = (|| -> Result<()> {
        {
            let (image, _) = gex_kernel::nt::boot::trampoline_input(class32);
            tramp_key = gex_cgen::image::image_key(&image);
            reuse(&web.join(&tramp_key), &sources)?;
            let t = nt_cgen::prepare_trampolines(class32, jobs)?;
            // The browser leaves the guest's code for a system call and
            // resumes after it, inside the trampolines.
            let failed = nt_cgen::prepare_trampoline_regions(class32, &t, jobs)?;
            if !failed.is_empty() {
                bail!("the trampoline page's resume points {failed:x?} have no region entry");
            }
            report.web_regions += gex_kernel::nt::boot::trampoline_resumes(class32).len();
        }
        for (path, base, bytes, interior) in inputs {
            let pe = gex_ld::pe::parse(bytes).map_err(|e| anyhow!("{path}: {e}"))?;
            let end = base + pe.image_size as u64;
            let mut interior = interior.clone();
            interior.extend(web_entries.iter().copied().filter(|a| a >= base && *a < end));
            let interior = &interior;
            let key_dir = nt_cgen::unit_dir(&pe, *base)?;
            if let Ok((image, _)) = gex_xlate::func::pe_input(&pe, *base) {
                identities.push(format!(
                    "{base:x}:{}:{}:{path}",
                    gex_cgen::image::image_key(&image),
                    gex_key::image_identity(&image).trim_start_matches("image:")
                ));
            }
            let how = if key_dir.join("c.wasm").exists() {
                "present"
            } else if reuse(&key_dir, &sources)? {
                "reused"
            } else {
                eprintln!("  building the web unit for {path} at {base:#x} ...");
                "built"
            };
            let p = nt_cgen::prepare_pe(&pe, *base, jobs, true).with_context(|| format!("building the web unit for {path}"))?;
            note_left(&pe, *base, &p, path, " (web)", report)?;
            if !interior.is_empty() {
                let failed = nt_cgen::prepare_regions(&pe, *base, &p, interior, jobs)?;
                if nt_cgen::region_module(&p).is_some() {
                    report.web_regions += interior.len() - failed.len();
                }
            }
            nt_cgen::prepare_providers(&pe)?;
            report.web_units.push((path.clone(), how));
        }
        Ok(())
    })();
    gex_cgen::image::set_unguarded(true);
    r?;
    // How the launcher starts the program: the guest's paths as recorded.
    let mut settings: Vec<String> = ntss
        .settings
        .iter()
        .filter(|s| s.split_once('=').is_some_and(|(k, _)| replayed_setting(k)))
        .cloned()
        .collect();
    // Stateful providers (Lua), the same modules as natively: the browser
    // runtime finds them under the unit cache.
    let mut spec = Vec::new();
    for (lib, wasm) in stateful {
        let rel = format!("stateful/{}", wasm.file_name().unwrap().to_string_lossy());
        std::fs::create_dir_all(web.join("stateful"))?;
        std::fs::copy(wasm, web.join(&rel)).with_context(|| format!("copying {}", wasm.display()))?;
        spec.push(format!("{}={rel}", lib.to_ascii_lowercase()));
    }
    if !spec.is_empty() {
        settings.push(format!("GEX_NT_STATEFUL={}", spec.join(",")));
    }
    if !identities.is_empty() {
        settings.push(format!("GEX_NT_IDENTITIES={}", identities.join(";")));
    }
    use crate::json::{b, obj, s, strs};
    let j = obj(vec![
        ("exe", s(&ntss.exe)),
        ("prefix", s(&ntss.prefix)),
        ("cwd", s(project.program.cwd.as_deref().unwrap_or("/"))),
        ("args", strs(args.unwrap_or(&ntss.args))),
        ("env", strs(&ntss.env)),
        ("settings", strs(&settings)),
        ("gl", b(ntss.gl)),
        // The units' directory in the project, and their flavour.
        ("cgen", s(web.strip_prefix(dir).unwrap_or(&web).to_string_lossy())),
        ("unguarded", b(!guarded)),
        ("trampolines", s(&tramp_key)),
        // What the page keeps across loads (IndexedDB): the Windows user
        // profiles, where a program's saves and settings go (AppData,
        // Documents).
        ("persist", s(format!("{}/drive_c/users", ntss.prefix.trim_end_matches('/')))),
        // user/eject.json "start": the app library's start() defaults.
        ("start", crate::json::Json::parse(start).map_err(|e| anyhow!("user/eject.json \"start\": {e}"))?),
    ]);
    std::fs::write(dir.join(WEB_NT_RUN), crate::json::write(&j))?;
    // The page's bundle: the rootfs as one tar (links followed), which the
    // page fetches into a SharedArrayBuffer.
    if dir.join(WEB_NT_PAGE).is_dir() {
        let tar = dir.join("build/web/rootfs.tar");
        let status = std::process::Command::new("tar")
            .arg("-C")
            .arg(dir.join("build/rootfs"))
            .arg("-chf")
            .arg(&tar)
            .arg(".")
            .status()
            .context("running tar")?;
        if !status.success() {
            bail!("tar of build/rootfs failed");
        }
    }
    Ok(())
}

/// GEX_NT_* settings a run replays from the session unless set now: those
/// that change what the program does (not diagnostics).
fn replayed_setting(name: &str) -> bool {
    !matches!(
        name,
        "GEX_NT_STATS" | "GEX_NT_PEEK" | "GEX_NT_TRACE" | "GEX_NT_TRACE_IMAGES" | "GEX_NT_HANG_DUMP" | "GEX_NT_HANG_SECS" | "GEX_NT_CGEN_TRACE"
            | "GEX_NT_CGEN_BUILD" | "GEX_NT_CGEN_SYNC"
    )
}

pub fn run(dir: &Path, flags: &crate::run::RunFlags) -> Result<Exit> {
    let ntss = nt_session(dir)?.ok_or_else(|| anyhow!("not a Windows program's project"))?;
    let project = Project::load(dir)?;
    let manifest = BuildManifest::load(dir)?;
    if crate::manifest::inputs_id(dir, &[])? != manifest.inputs {
        bail!("{} changed since it was built (gex-project.json or user/eject.json); run ./build.sh", dir.display());
    }
    let (config, _) = Config::load(dir)?;
    let root = std::fs::canonicalize(crate::manifest::rootfs(dir)).with_context(|| format!("{} (run ./build.sh)", dir.display()))?;
    let units = std::fs::canonicalize(units_dir(dir))?;
    for s in &ntss.settings {
        if let Some((k, v)) = s.split_once('=') {
            if replayed_setting(k) && std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
    }
    std::env::set_var("GEX_CGEN_CACHE", &units);
    std::env::set_var("GEX_PRECOMPILED", units.join("precompiled"));
    std::env::set_var("GEX_NT_CGEN_BUILD", "0");
    gex_native::libprov::set(lib_providers(dir, &config));
    let jit = flags.jit_fallback || config.jit_fallback;
    nt_cgen::set_mode(if jit { nt_cgen::Mode::Default } else { nt_cgen::Mode::Aot });
    let args: Vec<String> = match (&flags.args, &config.args) {
        (Some(a), _) => a.clone(),
        (None, Some(a)) => a.clone(),
        (None, None) => ntss.args.clone(),
    };
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| !v.is_empty() && v != "0");
    let env: Vec<(String, String)> =
        ntss.env.iter().filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let cfg = nt::Config {
        prefix: ntss.prefix.clone(),
        image_path: ntss.image_path.clone(),
        cmdline: String::new(),
        env,
        cwd: ntss.cwd.clone(),
        trace: flags.trace || flag("GEX_NT_TRACE"),
        strict: flag("GEX_NT_STRICT"),
        settings: Vec::new(),
    };
    let setup = gex_native::nt::NtSetup { abi: abi_of(&ntss)?, exe_path: ntss.exe.clone(), args: args.clone(), cfg, display_out: flags.display.clone() };
    let mut argv = vec![ntss.exe.clone()];
    argv.extend(args);
    let opts = RunOptions {
        args: argv,
        trace: flags.trace,
        verbose: flags.verbose,
        sysroot: Some(root),
        cwd: project.program.cwd.clone(),
        gl: ntss.gl,
        wayland_out: flags.display.clone(),
        aot_only: !jit,
        ..RunOptions::default()
    };
    let (exit, _) = gex_native::nt::run_nt_program(&opts, &gex_xlate::Options::default(), &setup)?;
    Ok(exit)
}

#[cfg(test)]
mod tests {
    use super::WEB_NT_JS;

    /// Every module the vendored runtime imports (`'./x.mjs'`) is vendored
    /// too: a new import the list missed broke every ejected web run.
    #[test]
    fn the_web_runtime_vendors_every_module_it_imports() {
        let have: Vec<&str> = WEB_NT_JS.iter().map(|(f, _)| *f).collect();
        for (f, text) in WEB_NT_JS {
            for q in ['\'', '"'] {
                for part in text.split(&format!("{q}./")).skip(1) {
                    let Some(end) = part.find(q) else { continue };
                    let m = &part[..end];
                    if m.ends_with(".mjs") {
                        assert!(have.contains(&m), "{f} imports ./{m}, which WEB_NT_JS leaves out");
                    }
                }
            }
        }
    }
}
