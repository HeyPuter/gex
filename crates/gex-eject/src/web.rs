//! The web build of a project: the browser shim (gex's runtime compiled to
//! wasm) running the same program, in Node (`run.mjs`) or a page
//! (the user's `user/index.html`, which calls `app.mjs`'s `start()`).
//!
//! An eject given `--web-shim` copies the shim into `runtime/web/`, beside the
//! `gex.mjs` its gexc was built with. `build.sh` then also translates the
//! program for it into `build/web/` and writes `build/web/manifest.json`,
//! which says everything the launcher (`runtime/web/eject.mjs`) needs: the
//! modules, the files, the arguments, the replacements and the guest ABI they
//! are called with.
//!
//! v1 runs a single process with a single thread (gex's single-Worker path,
//! with a plain memory): a session that forked or started threads gets a
//! manifest that says so, and the launcher refuses it.

use crate::build::{BuiltImage, BuiltReplacement};
use crate::json::{self, obj, Json};
use crate::project::{rootfs_rel, Project};
use anyhow::{anyhow, Context, Result};
use gex_abi::ctx;
use gex_native::aot::{Abi, REPL_BASE, REPL_DATA_BYTES, REPL_END, REPL_STACK_BYTES};
use std::path::Path;

pub const SHIM: &str = "runtime/web/gex_browser.wasm";
pub const MANIFEST: &str = "build/web/manifest.json";

/// The browser runtime's JavaScript, as built into this gexc.
pub const GEX_MJS: &str = include_str!("../../../shims/browser/js/gex.mjs");
/// The modules `gex.mjs` imports, vendored beside it.
pub const GEX_MJS_DEPS: [(&str, &str); 3] = [
    ("lazy-fetch.mjs", include_str!("../../../shims/browser/js/lazy-fetch.mjs")),
    ("xcache.mjs", include_str!("../../../shims/browser/js/xcache.mjs")),
    ("single.mjs", include_str!("../../../shims/browser/js/single.mjs")),
];

/// Why this project cannot have a web build, if it cannot.
pub fn unsupported(project: &Project) -> Option<String> {
    if project.processes > 1 {
        Some(format!(
            "the recorded session ran {} processes; the web build runs one (gex's single-Worker path)",
            project.processes
        ))
    } else if project.threads > 1 {
        Some(format!(
            "the recorded session ran {} threads; the web build runs one (gex's single-Worker path, \
             with a plain memory)",
            project.threads
        ))
    } else {
        None
    }
}

/// The replacement module the web build uses for a native one: its
/// `.web.wasm` sibling (linked for a plain memory, which `build.sh` makes),
/// else the same file.
pub fn web_module(dir: &Path, module: &str) -> String {
    let web = module.strip_suffix(".wasm").map(|m| format!("{m}.web.wasm"));
    match web {
        Some(w) if dir.join(&w).exists() => w,
        _ => module.to_string(),
    }
}

type Files = (Vec<(String, String)>, Vec<(String, u64)>, Vec<String>, Vec<(String, u64)>);

/// The rootfs as the launcher bundles it: `(guest, project-relative)` files,
/// executable modes, empty directories, and every file's mtime.
fn files(dir: &Path) -> Result<Files> {
    let root = dir.join("build/rootfs");
    let (mut files, mut modes, mut dirs, mut mtimes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d)
            .with_context(|| format!("reading {}", d.display()))?
            .collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        if entries.is_empty() && d != root {
            dirs.push(format!("/{}", d.strip_prefix(&root)?.display()));
        }
        for e in entries {
            let p = e.path();
            let meta = std::fs::metadata(&p).with_context(|| format!("reading {}", p.display()))?;
            if meta.is_dir() {
                stack.push(p);
                continue;
            }
            let guest = format!("/{}", p.strip_prefix(&root)?.display());
            let rel = p.strip_prefix(dir)?.display().to_string();
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 != 0 {
                modes.push((guest.clone(), 0o755));
            }
            // Kept by the eject and the build from the files the session read.
            use std::os::unix::fs::MetadataExt;
            mtimes.push((guest.clone(), meta.mtime().max(0) as u64));
            files.push((guest, rel));
        }
    }
    files.sort();
    dirs.sort();
    mtimes.sort();
    Ok((files, modes, dirs, mtimes))
}

/// Write `build/web/manifest.json`, for a build whose web modules are
/// `images` and whose replacements are `repl`.
pub fn manifest(
    dir: &Path,
    project: &Project,
    images: &[BuiltImage],
    pins: &[(String, u64, u64)],
    repl: &[BuiltReplacement],
    syscalls: Option<&std::collections::HashSet<u64>>,
    jit_fallback: bool,
) -> Result<()> {
    // A static program's image module owns the memory and tables. A dynamic
    // one's objects are all library modules (see `Translation::translate`),
    // started from a seed module that translates nothing.
    let own = images.iter().find(|i| i.kind == "image" && i.path == project.program.exe);
    let (module, closure, image_module): (String, Vec<&String>, Json) = match own {
        Some(i) => (
            i.module.clone(),
            images.iter().filter(|x| x.kind == "shared" && x.identity == i.identity).map(|x| &x.module).collect(),
            Json::Null,
        ),
        None => {
            let seed = "build/web/seed.wasm";
            std::fs::write(dir.join(seed), gex_xlate::seed_module(false).wasm)
                .with_context(|| format!("writing {seed}"))?;
            let img = images
                .iter()
                .find(|x| x.path == project.program.exe)
                .ok_or_else(|| anyhow!("internal: no web module for {}", project.program.exe))?;
            (
                seed.to_string(),
                images.iter().filter(|x| x.kind == "shared").map(|x| &x.module).collect(),
                json::s(&img.module),
            )
        }
    };
    let (files, modes, dirs, mtimes) = files(dir)?;
    let int_args: &[u8] = if gex_abi::X86 { &[7, 6, 2, 1, 8, 9] } else { &[0, 1, 2, 3, 4, 5, 6, 7] };
    let exe_file = format!("build/{}", rootfs_rel(&project.program.exe)?.display());
    let m = obj(vec![
        ("format", json::s("gex-eject-web")),
        ("version", json::n(1)),
        ("arch", json::s(gex_abi::ARCH_NAME)),
        ("jit_fallback", json::b(jit_fallback)),
        ("shim", json::s(SHIM)),
        ("module", json::s(&module)),
        ("closure", json::strs(closure)),
        ("image_module", image_module),
        ("exe", json::s(&project.program.exe)),
        ("exe_file", json::s(exe_file)),
        ("argv", json::strs(&project.program.argv)),
        ("env", json::strs(&project.program.env)),
        ("cwd", json::opt(project.program.cwd.as_ref())),
        ("dirs", json::strs(&dirs)),
        ("files", Json::Obj(files.into_iter().map(|(g, r)| (g, json::s(r))).collect())),
        ("modes", Json::Obj(modes.into_iter().map(|(g, m)| (g, json::n(m))).collect())),
        ("mtimes", Json::Obj(mtimes.into_iter().map(|(g, t)| (g, json::n(t))).collect())),
        (
            "pins",
            Json::Obj(
                pins.iter()
                    .map(|(p, base, span)| (p.clone(), obj(vec![("base", json::n(*base)), ("span", json::n(*span))])))
                    .collect(),
            ),
        ),
        (
            "replacements",
            Json::Arr(
                repl.iter()
                    .map(|r| {
                        obj(vec![
                            ("id", json::n(u64::from(r.id))),
                            ("module", json::s(web_module(dir, &r.module))),
                            ("export", json::s(&r.export)),
                            ("abi", json::s(r.abi.name())),
                            ("sig", json::s(&r.sig)),
                            ("symbol", json::s(&r.symbol)),
                            ("missing", json::b(r.missing)),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            "guest_abi",
            obj(vec![
                ("int_args", Json::Arr(int_args.iter().map(|r| json::n(ctx::x(*r) as u64)).collect())),
                ("fp_args", Json::Arr((0..8u8).map(|v| json::n(ctx::v(v) as u64)).collect())),
                ("ret", json::n(ctx::x(0) as u64)),
                ("fret", json::n(ctx::v(0) as u64)),
            ]),
        ),
        (
            "syscalls",
            match syscalls {
                None => Json::Null,
                Some(set) => obj(vec![
                    ("nr_offset", json::n(ctx::SYSCALL_NR as u64)),
                    ("allowed", Json::Arr(gex_native::aot::raw_syscalls(set).into_iter().map(json::n).collect())),
                ]),
            },
        ),
        (
            "region",
            obj(vec![
                ("end", json::n(REPL_END)),
                // The single Worker is thread slot 0.
                ("stack_top", json::n(REPL_BASE + REPL_DATA_BYTES + REPL_STACK_BYTES)),
            ]),
        ),
    ]);
    std::fs::write(dir.join(MANIFEST), json::write(&m)).with_context(|| format!("writing {MANIFEST}"))
}

/// The manifest of a project that has no web build, saying why.
pub fn unsupported_manifest(dir: &Path, why: &str) -> Result<()> {
    std::fs::create_dir_all(dir.join("build/web"))?;
    let m = obj(vec![("format", json::s("gex-eject-web")), ("version", json::n(1)), ("unsupported", json::s(why))]);
    std::fs::write(dir.join(MANIFEST), json::write(&m)).with_context(|| format!("writing {MANIFEST}"))
}

/// Check a replacement's web module links to the browser's plain memory.
pub fn check_module(dir: &Path, module: &str, export: &str, abi: Abi) -> Result<()> {
    let w = web_module(dir, module);
    let bytes = std::fs::read(dir.join(&w)).with_context(|| format!("reading {w}"))?;
    gex_native::aot::ReplacementModule::inspect_for(&bytes, false)
        .and_then(|m| m.check(export, abi))
        .with_context(|| format!("{w}, for the web build"))?;
    Ok(())
}

/// `runtime/web/eject.mjs`: the launcher, shared by Node and the page.
pub const EJECT_MJS: &str = r#"// Generated by `gexc eject`: run an ejected project on gex's browser runtime.
// Shared by run.mjs (Node) and index.html (a page); `read(path)` fetches a
// project file as bytes either way. See docs/eject.md in gex.
import { Gex, Env, metadataSection } from './gex.mjs';

export async function launch(m, read, { args = null, write, jitFallback = false, strace = false } = {}) {
  if (m.unsupported) throw new Error(`gex-eject: this project has no web build: ${m.unsupported}`);
  if (m.arch === 'x86_64') globalThis.GEX_ARCH = 'x86';
  class Out extends Env {
    // A copy: `bytes` may view the guest's memory, which moves on before an
    // asynchronous writer (Node's stdout on a pipe) has sent it.
    write(fd, bytes) { write(fd, bytes.slice()); }
  }
  const gex = new Gex(new Out());
  const files = {};
  for (const [guest, rel] of Object.entries(m.files)) files[guest] = await read(rel);
  // Each library as the page sends one: its metadata, the compiled module,
  // and for the program's own module the program it is the image of.
  const closure = [];
  for (const rel of m.closure) {
    const bytes = await read(rel);
    closure.push({
      meta: metadataSection(bytes),
      mod: await WebAssembly.compile(bytes),
      program: rel === m.image_module ? m.exe : undefined,
    });
  }
  const pins = {};
  for (const [p, { base, span }] of Object.entries(m.pins)) pins[p] = { base, span };
  const argv = args ? [m.argv[0] ?? m.exe, ...args] : m.argv;
  const replace = m.replacements.length ? await replacer(m, read) : undefined;
  // Only the system calls the recorded session made (user/eject.json
  // "syscalls"); anything else stops the run, naming it.
  let syscallGuard;
  if (m.syscalls) {
    const allowed = new Set(m.syscalls.allowed.map(BigInt));
    syscallGuard = (ctx, pc, memory) => {
      const nr = new DataView(memory.buffer).getBigUint64(ctx + m.syscalls.nr_offset, true);
      if (!allowed.has(nr)) {
        throw new Error(`gex-eject: system call ${nr} (at pc ${pc}) was never made by the recorded session, and this runtime allows only those; add it to "allow_syscalls" in user/eject.json`);
      }
    };
  }
  const entry = await gex.load(await read(m.shim), await read(m.module), await read(m.exe_file), files, argv, {
    cwd: m.cwd, env: m.env, dirs: m.dirs, modes: m.modes, mtimes: m.mtimes ?? {}, pins, closure, program: m.exe, exe: m.exe,
    jit: jitFallback || m.jit_fallback ? undefined : false, replace, syscallGuard, strace,
  });
  return await gex.run(entry);
}

// Library replacements: the user's wasm, in the guest's memory, called with
// the guest's argument registers (see gen/gex_replace.h).
async function replacer(m, read) {
  const compiled = {};
  for (const r of m.replacements) {
    if (!r.missing && !compiled[r.module]) compiled[r.module] = await WebAssembly.compile(await read(r.module));
  }
  let funcs = null;
  const sigOf = (s) => {
    const [, p, q] = s.match(/^\((.*)\) -> \((.*)\)$/);
    return { params: p ? p.split(', ') : [], results: q ? q.split(', ') : [] };
  };
  const sigs = m.replacements.map((r) => (r.abi === 'c' && !r.missing ? sigOf(r.sig) : null));
  // `imports` are the gex.* host functions the guest's own modules get: a
  // decompiled function (the "entry" ABI) calls them as translated code does.
  return (ctx, pc, id, memory, imports) => {
    const r = m.replacements[id];
    if (!r) throw new Error(`gex-eject: replacement #${id} at ${pc} is not in this build`);
    if (r.missing) {
      throw new Error(`gex-eject: ${r.symbol} belongs to a replaced library, and ${r.module} has no \`${r.export}\`: add it and rebuild`);
    }
    if (!funcs) {
      // The region is above everything the guest and the shim's arena use;
      // grown into only now, after the shim's arena has placed itself.
      const need = m.region.end - memory.buffer.byteLength;
      if (need > 0) memory.grow(Math.ceil(need / 65536));
      const inst = {};
      for (const [path, mod] of Object.entries(compiled)) {
        const i = new WebAssembly.Instance(mod, { env: { memory }, gex: imports ?? {} });
        if (i.exports.__stack_pointer) i.exports.__stack_pointer.value = m.region.stack_top | 0;
        inst[path] = i;
      }
      funcs = m.replacements.map((x) => (x.missing ? null : inst[x.module].exports[x.export]));
    }
    const f = funcs[id];
    if (r.abi === 'ctx' || r.abi === 'entry') {
      f(ctx);
      return;
    }
    const dv = new DataView(memory.buffer);
    const a = m.guest_abi;
    let ni = 0, nf = 0;
    const args = sigs[id].params.map((t) => {
      if (t === 'i32' || t === 'i64') {
        const v = dv.getBigUint64(ctx + a.int_args[ni++], true);
        return t === 'i32' ? Number(BigInt.asIntN(32, v)) : BigInt.asIntN(64, v);
      }
      const off = ctx + a.fp_args[nf++];
      return t === 'f32' ? dv.getFloat32(off, true) : dv.getFloat64(off, true);
    });
    const res = f(...args);
    const out = new DataView(memory.buffer);
    switch (sigs[id].results[0]) {
      case 'i32': out.setBigUint64(ctx + a.ret, BigInt(res >>> 0), true); break;
      case 'i64': out.setBigUint64(ctx + a.ret, BigInt.asUintN(64, BigInt(res)), true); break;
      case 'f32': out.setBigUint64(ctx + a.fret + 8, 0n, true); out.setBigUint64(ctx + a.fret, 0n, true); out.setFloat32(ctx + a.fret, res, true); break;
      case 'f64': out.setBigUint64(ctx + a.fret + 8, 0n, true); out.setFloat64(ctx + a.fret, res, true); break;
      default: break;
    }
  };
}
"#;

/// `run.mjs`: the Node launcher.
pub const RUN_MJS: &str = r#"#!/usr/bin/env node
// Run this ejected project on gex's browser runtime, under Node. Generated by
// `gexc eject`.
//
//   node run.mjs [--jit-fallback] [--strace] [--] [ARGS...]
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const opts = { write: (fd, b) => (fd === 2 ? process.stderr : process.stdout).write(b) };
let av = process.argv.slice(2);
while (av.length && av[0].startsWith('--')) {
  const f = av.shift();
  if (f === '--') break;
  else if (f === '--jit-fallback') opts.jitFallback = true;
  else if (f === '--strace') opts.strace = true;
  else { console.error(`run.mjs: unknown option ${f}`); process.exit(2); }
}
if (av.length || process.argv.includes('--')) opts.args = av;
let m;
try {
  m = JSON.parse(readFileSync(join(here, 'build/web/manifest.json'), 'utf8'));
} catch (e) {
  console.error(`run.mjs: no web build (${e.message}); run ./build.sh`);
  process.exit(1);
}
const { launch } = await import(pathToFileURL(join(here, 'runtime/web/eject.mjs')));
const read = async (p) => new Uint8Array(readFileSync(join(here, p)));
try {
  process.exitCode = await launch(m, read, opts);
} catch (e) {
  console.error(`run.mjs: ${e.message}`);
  process.exitCode = 1;
}
"#;

/// `serve.mjs`: a static server for the page (`user/index.html` at `/`, the
/// project's files beside it). gex's runtime needs
/// `SharedArrayBuffer`, so the page must be cross-origin isolated (the two
/// headers below); a plain static server is not enough.
pub const SERVE_MJS: &str = r#"#!/usr/bin/env node
// Serve this ejected project's page (user/index.html) with the headers gex's
// runtime needs (cross-origin isolation, for SharedArrayBuffer). Generated by
// `gexc eject`.   node serve.mjs [port]      then open http://localhost:PORT/
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { dirname, extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const port = Number(process.argv[2] ?? 8000);
const types = { '.html': 'text/html', '.mjs': 'text/javascript', '.js': 'text/javascript',
  '.json': 'application/json', '.wasm': 'application/wasm' };
const server = createServer(async (req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname));
  if (path.includes('..')) { res.writeHead(403).end(); return; }
  try {
    const file = path === '/' || path === '/index.html' ? 'user/index.html' : path;
    const body = await readFile(join(here, file));
    res.writeHead(200, {
      'Content-Type': types[extname(file)] ?? 'application/octet-stream',
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
    });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
// Port 0 is any free one; the line says which it got.
}).listen(port, () => console.log(`http://localhost:${server.address().port}/`));
"#;

/// `user/index.html`: the example page, the same for every kind of project
/// (a Linux program, an NT-layer one, a W3 page). Written once; the user's
/// from then on. Everything the program needs is the app library's
/// (`app.mjs`, `start()`): the page only gives it an element.
pub const SHELL_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>gex-eject</title>
<style>
  html, body { margin: 0; height: 100%; overflow: hidden; background: #111; color: #ddd; font: 14px system-ui, sans-serif; }
  #error { position: fixed; left: 1em; right: 1em; bottom: 1em; margin: 0; color: #f88; white-space: pre-wrap; }
</style>
</head>
<body>
<pre id="error"></pre>
<script type="module">
  // Your page: gexc eject wrote it once (user/index.html) and never rewrites
  // it. The program is app.mjs's: start() takes an element to fill and,
  // optionally, args, env, size ('fit' or {w, h}), onLog, onExit and more
  // (docs/eject.md, "The web build"); it gives back { exited, kill(), resize() }.
  import { start } from './app.mjs';
  const error = document.getElementById('error');
  const show = (e) => { error.textContent = String(e?.message ?? e); };
  try {
    const app = await start({ container: document.body });
    app.exited.then((status) => { if (status) show(`The program exited (${status}).`); }, show);
  } catch (e) {
    show(e);
  }
</script>
</body>
</html>
"#;

/// Fail if a built web directory has or names `nt.html`: gex's test page is
/// never part of an ejected build (the page is the user's).
pub fn check_no_test_page(dir: &Path) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let e = e?;
            let p = e.path();
            if e.file_type()?.is_dir() {
                stack.push(p);
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if name == "nt.html" {
                anyhow::bail!("{} is gex's test page, which an ejected build never ships", p.display());
            }
            if [".js", ".mjs", ".html", ".json"].iter().any(|x| name.ends_with(x)) {
                let b = std::fs::read(&p)?;
                if b.windows(7).any(|w| w == b"nt.html") {
                    anyhow::bail!("{} names nt.html (gex's test page), which an ejected build never ships", p.display());
                }
            }
        }
    }
    Ok(())
}

/// Write the example page to `user/index.html` unless the user has one.
pub fn write_shell(out: &Path) -> Result<()> {
    let p = out.join("user/index.html");
    if !p.exists() {
        crate::eject::write_file(&p, SHELL_HTML.as_bytes(), false)?;
    }
    Ok(())
}

/// `app.mjs` of a Linux program's project: the app library's `start()` on
/// the single-Worker launcher (`runtime/web/eject.mjs`), so the page is the
/// same as for every other kind of project.
pub const APP_MJS: &str = r#"// Generated by `gexc eject`: this project's app library (docs/eject.md,
// "The web build"). The page imports `start` and gives it an element:
//   const app = await start({ container: document.body });
//   const status = await app.exited;
// Options: container, args, env (['K=V'] or {K: V}), cwd, onLog(text, stream)
// (default: a <pre> in the container), onExit(status), strace, translation
// (the JIT fallback: what the build never saw is translated), baseUrl.
import { launch } from './runtime/web/eject.mjs';

const ABSENT = { compositor: 'a desktop', gl: 'OpenGL', hostx: 'an X server', mem64: 'a 64-bit guest memory' };

export async function start(opts = {}) {
  for (const [k, what] of Object.entries(ABSENT)) {
    if (opts[k] === true) throw new Error(`start({ ${k}: true }): a Linux program's web build has no ${what} (gex-eject's single-Worker runtime)`);
  }
  const base = new URL(opts.baseUrl ?? new URL('.', import.meta.url), globalThis.location?.href);
  const at = (p) => new URL(p, base).href;
  const read = async (p) => {
    const r = await fetch(at(p));
    if (!r.ok) throw new Error(`fetching ${p}: ${r.status}`);
    return new Uint8Array(await r.arrayBuffer());
  };
  const r = await fetch(at('build/web/manifest.json'), { cache: 'no-cache' });
  if (!r.ok) throw new Error('no build/web/manifest.json: run ./build.sh');
  let m = await r.json();
  if (m.unsupported) throw new Error(`gex-eject: this project has no web build: ${m.unsupported}`);
  if (opts.env) {
    const more = Array.isArray(opts.env) ? opts.env : Object.entries(opts.env).map(([k, v]) => `${k}=${v}`);
    const names = new Set(more.map((e) => e.split('=')[0]));
    m = { ...m, env: [...m.env.filter((e) => !names.has(e.split('=')[0])), ...more] };
  }
  if (opts.cwd) m = { ...m, cwd: opts.cwd };
  const state = { out: '', status: null, phase: 'running' };
  let pre = null;
  if (!opts.onLog) {
    pre = document.createElement('pre');
    pre.style.cssText = 'margin: 1em; white-space: pre-wrap; font: 14px monospace;';
    (opts.container ?? document.body).append(pre);
  }
  const dec = [new TextDecoder(), new TextDecoder()];
  const write = (fd, b) => {
    const text = dec[fd === 2 ? 1 : 0].decode(b, { stream: true });
    state.out += text;
    if (opts.onLog) opts.onLog(text, fd === 2 ? 'stderr' : 'stdout');
    else pre.append(text);
  };
  let settle;
  const killed = new Promise((res) => { settle = res; });
  const run = launch(m, read, { args: opts.args ?? null, write, jitFallback: opts.translation === true, strace: !!opts.strace });
  const exited = Promise.race([run, killed]).then((status) => {
    state.status = status;
    state.phase = 'done';
    opts.onExit?.(status);
    return status;
  });
  return {
    exited,
    state,
    // The run is on this thread: it stops being waited for and heard.
    kill() { opts = { ...opts, onLog: () => {}, onExit: null }; settle(137); },
    resize() { return null; },
    describe: () => null,
  };
}
"#;
