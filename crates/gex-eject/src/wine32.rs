//! Ejecting a 32-bit Wine page: a Windows program on Wine as native wasm32, recorded
//! in the browser (crates/gex-eject/README.md, "32-bit Wine pages").
//!
//! A 32-bit Wine program is not one process but Wine: the
//! wineserver, services, the program and its helpers are each a guest
//! process running `wine` (a native wasm32 program) with Wine's PE side
//! (`winepe.wasm`) loading the Windows images, whose code is function-tier
//! units keyed by content. It runs in the same page (`nt.html`) a gex demo
//! does, so its project is that page, built with the features the user keeps
//! (`user/eject.json` `"features"`, crate::features), plus what the
//! recording says it used:
//!
//! ```text
//! project/
//!   gex-project.json    how the page starts the program; its processes,
//!                       units and wasm modules, as recorded        generated
//!   session/            wine32-session.json, the recording             generated
//!   bundle/rootfs.tar   the filesystem the page fetched            generated
//!   units/KEY/          the units the recording used               generated
//!   runtime/web/gex/    the browser shim, kernel and seed          generated
//!   build.sh            the build step                             generated
//!   user/eject.json     yours: features, ...                       created once
//!   build/web/          the site's inputs (build.sh): page/, rootfs.tar,
//!                       units/, wine32.json
//! ```
//!
//! Recording: `tools/eject/record-wine32.mjs` (headless Chromium) runs a 32-bit Wine page
//! to a window title and writes `wine32-session.json`. The page is built by the
//! gex checkout the project names (`page_source`, `web/build-eject-page.sh`),
//! since it is the features that decide what goes into it; the runtime's
//! wasm is the project's own copy. `web/eject-site.mjs PROJECT OUT` makes
//! the static site.

use crate::config::Config;
use crate::eject::{prepare, sha256, write_file};
use crate::json::{self, obj, s, strs, Json};
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

/// What the recorder writes.
pub const SESSION: &str = "wine32-session.json";
/// What makes a project a 32-bit Wine one.
pub const KIND: &str = "wine32";
/// A 64-bit Wine one (its loader under `x86_64-unix`): the same project and
/// build, with the 64-bit trampolines, the memory64 runtime beside the
/// 32-bit one, and the D3D device when the recorded page had it.
pub const KIND64: &str = "wine64";
/// What a build made before the rename wrote, still read.
pub const KIND_OLD: &str = "w3";
/// A recording's format, and the name recordings made before the rename carry.
pub const FORMAT: &str = "gex-wine32-session";
pub const FORMAT_OLD: &str = "gex-w3-session";

/// The runtime's wasm the page loads from `gex/` (web/build-eject-page.sh).
const RUNTIME_WASM: &[&str] = &["gex_browser.x86.shared.wasm", "gex_kernel.x86.wasm", "seed.x86.shared.wasm"];
/// The memory64 runtime (`GEX_MEM64=1 shims/browser/build.sh`) a 64-bit Wine
/// page runs a program marked for memory64 on.
const RUNTIME_WASM_M64: &[&str] = &["gex_browser.x86.m64.shared.wasm", "seed.x86.m64.shared.wasm"];

/// Whether a recorded page runs 64-bit Wine: its loader is Wine's
/// `x86_64-unix` one.
fn is_wine64(page: &Json) -> bool {
    json::opt_str(page, "exe").is_some_and(|e| e.contains("/x86_64-unix/"))
}

/// The runtime wasm a page of this kind ships.
fn runtime_wasm(wine64: bool) -> Vec<&'static str> {
    let mut v = RUNTIME_WASM.to_vec();
    if wine64 {
        v.extend_from_slice(RUNTIME_WASM_M64);
    }
    v
}

/// Whether `record` holds a 32-bit Wine recording.
pub fn is_recording(record: &Path) -> bool {
    record.join(SESSION).is_file()
}

/// Whether `dir` is a 32-bit Wine project.
pub fn is_project(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(crate::project::PROJECT_FILE))
        .ok()
        .and_then(|t| Json::parse(&t).ok())
        .is_some_and(|v| matches!(v.get("kind").and_then(|k| k.as_str()), Some(KIND | KIND_OLD | KIND64)))
}

pub struct Wine32Options {
    /// The gex checkout's `web/` directory: the page's source and
    /// `build-eject-page.sh`, and the built runtime under `public/gex/`.
    pub web_src: PathBuf,
    /// Where the units the recording names are (a unit cache: KEY/c.wasm).
    pub unit_cache: Vec<PathBuf>,
    /// The runtime's features a new project's `user/eject.json` switches off.
    pub features_off: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Wine32Summary {
    pub processes: usize,
    pub units: usize,
    pub unit_bytes: u64,
    pub bundle_bytes: u64,
    pub modules: usize,
    pub providers: usize,
    pub kept_user: bool,
}

/// The provider modules (`providers/NAME-FINGERPRINT.wasm`, a unit cache's
/// layout) that `units` (key, guest path) bind in `bundle`: each image's
/// export providers that bind one of its exports, and the trampoline
/// page's when any image is 32-bit.
fn provider_files(bundle: &[u8], units: &[(String, String)]) -> Result<Vec<String>> {
    use gex_cgen::provider;
    let tar = crate::reach::Tar::parse(bundle)?;
    let file = provider_file;
    let mut out = std::collections::BTreeSet::new();
    let mut class32 = false;
    for (_, path) in units {
        let Some(data) = tar.read(path) else { continue };
        let Ok(pe) = gex_ld::pe::parse(data) else { continue };
        class32 |= !pe.is64;
        let exports: std::collections::HashSet<&str> = pe
            .exports
            .entries
            .iter()
            .filter(|e| e.rva != 0 && e.forwarder.is_none())
            .flat_map(|e| e.names.iter().map(|n| n.as_str()))
            .collect();
        for p in provider::for_image(&pe.exports.dll_name, !pe.is64, data) {
            if p.binds.iter().any(|(_, e)| exports.contains(e.as_str())) {
                out.insert(file(&p));
            }
        }
    }
    if class32 {
        out.extend(provider::for_dll(provider::TRAMPOLINES_DLL, true).iter().filter(|p| !p.binds.is_empty()).map(file));
    }
    Ok(out.into_iter().collect())
}

/// Whether any of `units` (key, guest path) is a 32-bit image in `bundle`.
fn class32(bundle: &[u8], units: &[(String, String)]) -> Result<bool> {
    let tar = crate::reach::Tar::parse(bundle)?;
    Ok(units.iter().any(|(_, path)| tar.read(path).and_then(|d| gex_ld::pe::parse(d).ok()).is_some_and(|pe| !pe.is64)))
}

/// The guest path a project names the NT trampoline page's unit by.
pub(crate) const TRAMPOLINES_PATH: &str = "[nt trampolines]";

/// The keys a 32-bit Wine page may ask for the 32-bit NT trampoline page's unit by:
/// 32-bit Wine's KUSER fold on (`gexc cgen-image --wine32-kuser --nt-trampolines32`),
/// guarded or not (see `rsrc::FLAVORS`).
fn trampoline_keys() -> [String; 2] {
    let (image, _) = gex_kernel::win::trampoline_input(true);
    [false, true].map(|unguarded| crate::rsrc::key_with(&image, (unguarded, true)))
}

/// The keys a 64-bit Wine page may ask for the 64-bit NT trampoline page's unit by:
/// `gexc cgen-image --nt-trampolines64`, with `--mem64` for a memory64 process
/// and without for Wine's other processes, and with `--wine64-compact` (a
/// wasm32 memory) for a process in the compact space, guarded or not.
fn trampoline_keys64() -> Vec<String> {
    let (image, _) = gex_kernel::win::trampoline_input(false);
    let mut keys: Vec<String> =
        crate::rsrc::flavors_mem().filter(|((_, kuser), _)| !kuser).map(|(g, m)| crate::rsrc::key_with_mem(&image, g, m)).collect();
    keys.extend([false, true].map(|unguarded| crate::rsrc::key_with_entry(&image, (unguarded, true), true)));
    keys
}

/// Adds the 64-bit trampoline page's units the caches hold; none fails the eject.
fn add_trampolines64(units: &mut Vec<(String, String)>, caches: &[PathBuf]) -> Result<()> {
    let keys = trampoline_keys64();
    let held = |k: &str| caches.iter().any(|c| c.join(k).join("c.wasm").is_file() && c.join(k).join("c.entries").is_file());
    let found: Vec<&String> = keys.iter().filter(|k| held(k)).collect();
    if found.is_empty() {
        bail!(
            "no unit of the 64-bit NT trampolines ({}) in the unit cache ({}): build them with GEX_CGEN_CACHE=<the unit cache> gexc cgen-image --nt-trampolines64 [--mem64 | --wine64-compact]",
            keys.join(" or "),
            caches.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":")
        );
    }
    for k in found {
        if !units.iter().any(|(u, _)| u == k) {
            units.push((k.clone(), TRAMPOLINES_PATH.to_string()));
        }
    }
    Ok(())
}

/// Adds the trampoline page's unit to `units`, each of its keys a cache in
/// `caches` holds and `units` does not name yet; none in any cache fails
/// the eject, with the command that builds it.
fn add_trampolines(units: &mut Vec<(String, String)>, caches: &[PathBuf]) -> Result<()> {
    let keys = trampoline_keys();
    let held = |k: &str| caches.iter().any(|c| c.join(k).join("c.wasm").is_file() && c.join(k).join("c.entries").is_file());
    let found: Vec<&String> = keys.iter().filter(|k| held(k)).collect();
    if found.is_empty() {
        bail!(
            "no unit of the NT trampolines ({}) in the unit cache ({}): build it with GEX_CGEN_CACHE=<the unit cache> gexc cgen-image --wine32-kuser --nt-trampolines32",
            keys.join(" or "),
            caches.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":")
        );
    }
    for k in found {
        if !units.iter().any(|(u, _)| u == k) {
            units.push((k.clone(), TRAMPOLINES_PATH.to_string()));
        }
    }
    Ok(())
}

/// The project's units (wine32.json `units`): each with its recorded `at` and
/// `base`. The trampolines' unit, added by the eject (no recorded entry),
/// is used by every 32-bit process from its first system call: `at` 0, so
/// the page compiles it with the boot's units and packs it in the boot
/// archive (web/eject-site.mjs), never as a unit compiled when first mapped,
/// which no process maps.
fn units_json(recorded: &[Json], units: &[(String, String)]) -> Json {
    Json::Arr(units.iter().enumerate().map(|(i, (k, p))| {
        let mut kv = vec![("key", s(k)), ("path", s(p))];
        let u = recorded.get(i).filter(|u| json::opt_str(u, "key").as_deref() == Some(k.as_str()));
        match u.and_then(|u| u.get("at")).filter(|a| matches!(a, Json::Num(_))) {
            Some(at) => kv.push(("at", at.clone())),
            None if p == TRAMPOLINES_PATH => kv.push(("at", json::n(0))),
            None => {}
        }
        if let Some(b) = u.and_then(|u| u.get("base")).filter(|b| matches!(b, Json::Str(_))) {
            kv.push(("base", b.clone()));
        }
        obj(kv)
    }).collect())
}

/// A provider module's file in a unit cache, as a 32-bit Wine page asks for it. The
/// page runs with 32-bit Wine's KUSER_SHARED_DATA fold on (`gexc cgen-image
/// --wine32-kuser`), and a provider's fingerprint hashes the C header, which has
/// the fold's helpers only then; the fold is set for the call and put back,
/// as `rsrc.rs` `abi_hash` does.
fn provider_file(p: &gex_cgen::provider::Provider) -> String {
    let was = gex_abi::kuser_fold();
    gex_abi::set_kuser_fold(true);
    let f = gex_cgen::provider::fingerprint(std::slice::from_ref(p));
    gex_abi::set_kuser_fold(was);
    format!("providers/{}-{f}.wasm", p.name)
}

fn link_or_copy(from: &Path, to: &Path) -> Result<u64> {
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    if std::fs::hard_link(from, to).is_err() {
        std::fs::copy(from, to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    }
    Ok(std::fs::metadata(to)?.len())
}

fn list<'a>(v: &'a Json, k: &str) -> &'a [Json] {
    v.get(k).and_then(|x| x.as_arr()).unwrap_or(&[])
}

/// `gexc eject RECORD --out PROJECT` for a 32-bit Wine recording.
pub fn eject(record: &Path, out: &Path, runtime: &Path, o: &Wine32Options) -> Result<Wine32Summary> {
    let session_path = record.join(SESSION);
    let text = std::fs::read_to_string(&session_path).with_context(|| format!("reading {}", session_path.display()))?;
    let v = Json::parse(&text).map_err(|e| anyhow!("{}: {e}", session_path.display()))?;
    let what = SESSION;
    if !matches!(json::opt_str(&v, "format").as_deref(), Some(FORMAT | FORMAT_OLD)) {
        bail!("{what}: not a wine32 session (format \"{FORMAT}\")");
    }
    let page = json::field(&v, "page", what)?;
    json::str_field(page, "exe", what)?;
    let bundle = json::opt_str(&v, "bundle").ok_or_else(|| {
        anyhow!("{what}: no \"bundle\": record with --bundle PATH, the tar the page fetched")
    })?;
    let mut units: Vec<(String, String)> = list(&v, "units")
        .iter()
        .map(|u| Ok((json::str_field(u, "key", what)?, json::str_field(u, "path", what)?)))
        .collect::<Result<_>>()?;
    if units.iter().any(|(k, _)| k.len() != 64 || !k.bytes().all(|c| c.is_ascii_hexdigit())) {
        bail!("{what}: a unit key that is not 64 hex digits");
    }
    let bundle_bytes = std::fs::read(&bundle).with_context(|| format!("reading the bundle {bundle}"))?;
    // The NT trampoline page's unit, which is no image's and so in no
    // recording's units: a 32-bit program's system calls go through it, and
    // an AOT-only page has no other code for them.
    let wine64 = is_wine64(page);
    if wine64 {
        add_trampolines64(&mut units, &o.unit_cache)?;
    } else if class32(&bundle_bytes, &units)? {
        add_trampolines(&mut units, &o.unit_cache)?;
    }
    for f in runtime_wasm(wine64) {
        if !o.web_src.join("public/gex").join(f).is_file() {
            bail!("{}: no public/gex/{f}: build the shim and kernel first (shims/browser/build.sh, shims/kernel/build.sh)", o.web_src.display());
        }
    }
    if !o.web_src.join("build-eject-page.sh").is_file() {
        bail!("{} is not gex's web/ directory (no build-eject-page.sh)", o.web_src.display());
    }
    crate::features::check(&o.features_off)?;

    let kept_user = prepare(out)?;
    let mut sum = Wine32Summary { kept_user, ..Wine32Summary::default() };

    // The recording, as it was.
    write_file(&out.join("session").join(SESSION), text.as_bytes(), false)?;
    // The bundle, whole: Wine's prefix and every file a process may open.
    let bundle_sha = sha256(&std::fs::read(&bundle).with_context(|| format!("reading the bundle {bundle}"))?);
    sum.bundle_bytes = link_or_copy(Path::new(&bundle), &out.join("bundle/rootfs.tar"))?;
    // The units, from the cache, each checked to be there.
    for (key, path) in &units {
        let src = o.unit_cache.iter().map(|c| c.join(key)).find(|d| d.join("c.wasm").is_file() && d.join("c.entries").is_file());
        let Some(src) = src else {
            bail!("no unit {key} ({path}) in the unit cache ({}): give --unit-cache DIR or GEX_EJECT_UNIT_CACHE",
                o.unit_cache.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":"));
        };
        for f in ["c.wasm", "c.entries"] {
            sum.unit_bytes += link_or_copy(&src.join(f), &out.join("units").join(key).join(f))?;
        }
        // Its region entries, when the cache has them (`gexc cgen-image
        // --entries`): where a run without translation resumes inside it.
        let regions = src.parent().map(|c| c.join("regions").join(key));
        if let Some(r) = regions.filter(|r| r.join("c.wasm").is_file() && r.join("c.entries").is_file()) {
            for f in ["c.wasm", "c.entries"] {
                sum.unit_bytes += link_or_copy(&r.join(f), &out.join("units/regions").join(key).join(f))?;
            }
        }
    }
    sum.units = units.len();
    // The provider modules the units bind (as the page picks them, cgen.rs
    // `unit_key`), and the trampolines' for a 32-bit program: an AOT-only
    // page has no other code for them.
    for file in provider_files(&bundle_bytes, &units)? {
        let src = o.unit_cache.iter().map(|c| c.join(&file)).find(|p| p.is_file());
        // A 64-bit page runs without a provider it is not given (the image's
        // own code, translated): said, not refused.
        if src.is_none() && wine64 {
            eprintln!("gexc eject: no provider module {file} in the unit cache: the page runs the image's own code for it");
            continue;
        }
        let Some(src) = src else {
            bail!("no provider module {file} in the unit cache ({}): build it with GEX_CGEN_CACHE=<the unit cache> gexc cgen-image --providers",
                o.unit_cache.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(":"));
        };
        sum.unit_bytes += link_or_copy(&src, &out.join("units").join(&file))?;
        sum.providers += 1;
    }
    // The runtime: the browser shim, kernel and seed this page runs, and the
    // gexc that builds it.
    for f in runtime_wasm(wine64) {
        link_or_copy(&o.web_src.join("public/gex").join(f), &out.join("runtime/web/gex").join(f))?;
    }
    link_or_copy(runtime, &out.join("runtime/gexc"))?;

    let processes = list(&v, "processes");
    let modules = list(&v, "modules");
    sum.processes = processes.len();
    sum.modules = modules.len();
    let web_src = std::fs::canonicalize(&o.web_src).unwrap_or_else(|_| o.web_src.clone());
    let project = obj(vec![
        ("kind", s(if wine64 { KIND64 } else { KIND })),
        ("about", s("A Wine page (Wine's Unix side as native wasm) ejected by gexc eject; see crates/gex-eject/README.md, \"Wine pages\". Generated: rewritten by every eject.")),
        ("page", page.clone()),
        ("title", v.get("title").cloned().unwrap_or(Json::Null)),
        ("bundle", obj(vec![("file", s("bundle/rootfs.tar")), ("sha256", s(&bundle_sha)), ("from", s(&bundle))])),
        ("processes", Json::Arr(processes.to_vec())),
        // Each with `at`, when the recording first used it (seconds; none
        // from an older recorder): a unit first used after the title is
        // compiled when a process maps it (web/eject-site.mjs, `demand`).
        ("units", units_json(list(&v, "units"), &units)),
        ("title_seconds", v.get("title_seconds").cloned().unwrap_or(Json::Null)),
        ("unit_misses", Json::Arr(list(&v, "unit_misses").to_vec())),
        ("uncovered", Json::Arr(list(&v, "uncovered").to_vec())),
        ("modules", Json::Arr(modules.to_vec())),
        // The files the run opened (the recorder's `files_read`; null from an
        // older recorder): what `"bundle": "split"` keeps in the bundle.
        ("files_read", v.get("files_read").cloned().unwrap_or(Json::Null)),
        // Those it had opened by its title window (`files_read_title`): what
        // `"bundle": "title"` keeps.
        ("files_read_title", v.get("files_read_title").cloned().unwrap_or(Json::Null)),
        // When each file was first read, and when each image was first
        // loaded: a cut later than the window (`"title_after"`), and the
        // images it only opened, which it leaves out.
        ("files_read_at", v.get("files_read_at").cloned().unwrap_or(Json::Null)),
        ("images_loaded", v.get("images_loaded").cloned().unwrap_or(Json::Null)),
        // The functions the run called, per unit (the recorder's `coverage`,
        // from instrumented units: key -> slot ranges; null without): what
        // `"cold"` keeps in each unit's primary module.
        ("coverage", v.get("coverage").cloned().unwrap_or(Json::Null)),
        // What wineboot made that the prefix does not save (the recorder's
        // `volatile_keys`, from a page run with WINEGEX_RECORD_HARDWARE=1;
        // null without): what `"wineboot": "skip"` writes into the prefix.
        ("volatile_keys", v.get("volatile_keys").cloned().unwrap_or(Json::Null)),
        ("page_source", s(web_src.display().to_string())),
        ("session", s(sha256(text.as_bytes()))),
    ]);
    write_file(&out.join(crate::project::PROJECT_FILE), json::write(&project).as_bytes(), false)?;
    write_file(&out.join("build.sh"), BUILD_SH.as_bytes(), true)?;
    write_file(&out.join(".gitignore"), b"/build/\n", false)?;
    if !out.join(crate::config::CONFIG_FILE).exists() {
        // A desktop shell (explorer.exe as the program) runs Wine's helpers
        // itself; a game does not need them.
        let exe = json::str_field(page, "exe", what)?;
        let shell = strip_dir(&exe).eq_ignore_ascii_case("explorer.exe");
        write_file(&out.join(crate::config::CONFIG_FILE), template(&o.features_off, wine64, !shell).as_bytes(), false)?;
    }
    // The page: the user's, an example to start from (app.mjs's start()).
    crate::web::write_shell(out)?;
    Ok(sum)
}

const BUILD_SH: &str = "#!/bin/sh
# Generated by `gexc eject`: build this wine32 page's site inputs into build/web
# (the page with user/eject.json's features, the bundle, the units).
# Then: node <gex>/web/eject-site.mjs . SITE
set -e
cd \"$(dirname \"$0\")\"
exec runtime/gexc eject build .
";

/// A 32-bit Wine project's `user/eject.json`: the features, the ones given off.
fn template(off: &[String], wine64: bool, strip: bool) -> String {
    json::write(&obj(vec![
        (
            "about",
            s("Your settings; gexc eject never rewrites this file. \"features\": the parts of gex's \
               runtime the build keeps, false leaves one out (crates/gex-eject/README.md, \"Features\"); \
               rebuild with ./build.sh. \"bundle\": \"split\" ships the files the recorded run \
               opened in the site's bundle and the rest as lazy files, fetched when the program \
               first reads one (\"title\": only those it opened up to its title window, a game's \
               default, with \"title_after\": seconds after the window to cut at instead, for a game whose \
               title menu comes later; \"whole\": everything in the bundle); see crates/gex-eject/README.md, \
               \"The bundle split\". \"cold\": \"none\" ships every unit whole in one archive; a clustering \
               (\"chunk:256\", ...) splits off each unit's code the recording never ran and \
               fetches it on first call, \"instrument\" builds units that record the functions a \
               run calls; see crates/gex-eject/README.md, \"Cold code\". \"resources\": \"trim\" ships no \
               resource the site's locale and DPI cannot load (\"whole\": every one); see \
               crates/gex-eject/README.md, \"Resources\". \"helpers\": \"strip\" leaves out the Wine \
               helper programs and services a game does not need (\"keep\": the prefix as recorded); \
               see crates/gex-eject/README.md, \"Helpers\". \"desktop\": \"none\" runs the program \
               with no desktop process, for a fullscreen game (\"explorer\": Wine's); see \
               crates/gex-eject/README.md, \"Desktop\". \"wineboot\": \"skip\" boots the session \
               without wineboot, from a recording made with WINEGEX_RECORD_HARDWARE=1 (\"run\": \
               Wine's); see crates/gex-eject/README.md, \"Wineboot\". \
               \"start\": defaults for the app library's start() \
               (args, env, cwd, size, mtimes, strace, trapstack, persist, ready), over the \
               recording's; the page's own options win (crates/gex-eject/README.md, \"The web build\")."),
        ),
        (
            "features",
            obj(crate::features::FEATURES.iter().map(|f| (f.name, json::b(f.name != "translation" && !off.iter().any(|x| x == f.name)))).collect()),
        ),
        // A game's bundle is what it reads up to its title; everything after
        // is fetched when first read.
        ("bundle", s(if strip { "title" } else { "split" })),
        ("title_after", json::n(0)),
        ("cold", s(COLD_DEFAULT)),
        // A 64-bit game's bundle is large and its own resources one
        // language already: whole until trimming earns its pass.
        ("resources", s(if wine64 { "whole" } else { "trim" })),
        ("helpers", s(if strip { "strip" } else { "keep" })),
        ("desktop", s(if strip { "none" } else { "explorer" })),
        ("wineboot", s("run")),
        ("start", obj(vec![])),
    ]))
}

/// Whether the page runs ahead-of-time code only: `"translation"` off, the
/// default unless `"cold": "jit"` (the function JIT's cold code) or an
/// explicit `"translation": true` keeps a translator.
fn aot_only(config: &Config) -> Result<bool> {
    let on = config.features_on.iter().any(|x| x == "translation");
    let off = config.features_off.iter().any(|x| x == "translation");
    let cold_jit = matches!(config.cold.as_deref(), Some("jit" | "tier1"));
    if off && cold_jit {
        bail!(
            "user/eject.json: \"translation\": false, and \"cold\": \"jit\" leaves the cold code to the \
             function JIT: leave \"translation\" unset for the function JIT alone, or pick another \"cold\""
        );
    }
    Ok(!on && !cold_jit)
}

/// The site's `identities` (`base:key:identity:path;...`): for each unit, its
/// image as the site ships it (web/rootfs.tar, trimmed or not) at the base
/// the unit was built for (found from the unit's addresses). A line is
/// written only when the key this gexc computes for that image is the unit's
/// own, so the page never takes a key its own hashing would not give. A
/// 64-bit memory's unit's base is written `m<base>` (the shim's `KNOWN`).
fn unit_identities(dir: &Path, web: &Path, units: &[Json], site_key: &dyn Fn(&str) -> String) -> Result<String> {
    let what = crate::project::PROJECT_FILE;
    let bytes = std::fs::read(web.join("rootfs.tar"))?;
    let tar = crate::reach::Tar::parse(&bytes)?;
    let mut lines = Vec::new();
    let mut skipped = 0;
    for u in units {
        let key = json::str_field(u, "key", what)?;
        let path = json::str_field(u, "path", what)?;
        let site = site_key(&key);
        let found = (|| -> Option<String> {
            let data = tar.read(&path)?;
            let pe = gex_ld::pe::parse(data).ok()?;
            let wasm = std::fs::read(dir.join("units").join(&key).join("c.wasm")).ok()?;
            let unit = gex_split::Unit::parse(&wasm).ok()?;
            let addrs: std::collections::HashSet<u64> = unit.addresses().into_iter().collect();
            let base = crate::reach::find_base(&pe, &addrs)?;
            let image = gex_xlate::func::pe_input(&pe, base).ok()?.0;
            let (_, m64) = crate::rsrc::flavors_mem().find(|&(g, m)| crate::rsrc::key_with_mem(&image, g, m) == site)?;
            let flavor = if m64 { "m" } else { "" };
            Some(format!("{flavor}{base:x}:{site}:{}:{path}", gex_key::image_identity(&image).trim_start_matches("image:")))
        })();
        match found {
            Some(l) => lines.push(l),
            None => skipped += 1,
        }
    }
    eprintln!("gexc eject build: identities for {} units ({skipped} left to the page to hash)", lines.len());
    Ok(lines.join(";"))
}

/// `"resources": "trim"` (crate::rsrc): writes `web/rootfs.tar`, the bundle
/// with every distinct PE trimmed, and `build/resources.json`. Returns the
/// units' new keys, old to new. An image whose unit's key this gexc does not
/// reproduce is shipped whole, at every path that holds it.
fn trim_resources(dir: &Path, v: &Json, units: &[Json], web: &Path) -> Result<std::collections::HashMap<String, String>> {
    use std::collections::{HashMap, HashSet};
    let what = crate::project::PROJECT_FILE;
    let bytes = std::fs::read(dir.join("bundle/rootfs.tar"))?;
    let tar = crate::reach::Tar::parse(&bytes)?;
    let env = json::str_list(json::field(v, "page", what)?, "env", what)?;
    let prefix = env.iter().find_map(|e| e.strip_prefix("WINEPREFIX=")).unwrap_or("/pfx").to_string();
    let site = crate::rsrc::site(&env, &tar, &prefix);
    // Each distinct file once, in parallel.
    let files = tar.files();
    let mut distinct: HashMap<String, (usize, usize)> = HashMap::new();
    for &(_, o, n) in &files {
        if bytes[o..o + n].starts_with(b"MZ") {
            distinct.entry(sha256(&bytes[o..o + n])).or_insert((o, n));
        }
    }
    let jobs: Vec<(String, (usize, usize))> = distinct.into_iter().collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done: std::sync::Mutex<HashMap<String, (Vec<u8>, crate::rsrc::Stat)>> = Default::default();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some((h, (o, n))) = jobs.get(i) else { break };
                if let Some(t) = crate::rsrc::trim(&bytes[*o..*o + *n], &site) {
                    done.lock().unwrap().insert(h.clone(), t);
                }
            });
        }
    });
    let mut trimmed = done.into_inner().unwrap();
    // The units of trimmed images: under the key of the trimmed image, when
    // this gexc computes the recorded key for the image as it was.
    let mut found: Vec<(String, String, String)> = Vec::new();
    let mut whole: HashSet<String> = HashSet::new();
    for u in units {
        let key = json::str_field(u, "key", what)?;
        let path = json::str_field(u, "path", what)?;
        let Some(orig) = tar.read(&path) else { continue };
        let h = sha256(orig);
        let Some((t, _)) = trimmed.get(&h) else { continue };
        let wasm = std::fs::read(dir.join("units").join(&key).join("c.wasm"))?;
        match crate::rsrc::rekey(&wasm, orig, t, &key)? {
            Some(new) => found.push((key, new, h)),
            None => {
                eprintln!("gexc eject build: \"resources\": \"trim\": {path}: its unit's key is not one this gexc makes for it (a project built by another gexc than its own?); shipped whole");
                whole.insert(h);
            }
        }
    }
    trimmed.retain(|h, _| !whole.contains(h));
    let rekeyed: HashMap<String, String> = found.into_iter().filter(|(_, _, h)| !whole.contains(h)).map(|(k, n, _)| (k, n)).collect();
    // The bundle, each trimmed file's bytes in place (sizes are unchanged).
    let mut out = bytes.clone();
    let mut rows = Vec::new();
    let (mut lang, mut dpi, mut dead, mut files_n) = (0usize, 0usize, 0usize, 0usize);
    let mut counted: HashSet<&str> = HashSet::new();
    for &(p, o, n) in &files {
        if !bytes[o..o + n].starts_with(b"MZ") {
            continue;
        }
        let h = sha256(&bytes[o..o + n]);
        let Some((t, st)) = trimmed.get(&h) else { continue };
        out[o..o + n].copy_from_slice(t);
        files_n += 1;
        if counted.insert(trimmed.get_key_value(&h).unwrap().0.as_str()) {
            lang += st.languages;
            dpi += st.dpi;
            dead += st.dead;
        }
        rows.push(obj(vec![
            ("path", s(p)),
            ("languages", json::n(st.languages as u64)),
            ("dpi", json::n(st.dpi as u64)),
            ("dead", json::n(st.dead as u64)),
            ("entries", json::n(st.entries as u64)),
        ]));
    }
    write_file(&web.join("rootfs.tar"), &out, false)?;
    eprintln!(
        "gexc eject build: \"resources\": \"trim\" (language {}, DPI {}): {files_n} files, {} distinct; \
         zeroed {lang} bytes of other languages, {dpi} of ribbon images for other DPIs, {dead} no directory reached; {} units rekeyed",
        site.lang.map_or("kept".into(), |l| format!("{l:#x}")),
        site.dpi.map_or("kept".into(), |d| d.to_string()),
        trimmed.len(),
        rekeyed.len()
    );
    write_file(
        &dir.join("build/resources.json"),
        json::write(&obj(vec![
            ("lang", site.lang.map_or(Json::Null, |l| json::n(l as u64))),
            ("dpi", site.dpi.map_or(Json::Null, |d| json::n(d as u64))),
            ("languages", json::n(lang as u64)),
            ("dpi_bytes", json::n(dpi as u64)),
            ("dead", json::n(dead as u64)),
            ("rekeyed", Json::Obj(rekeyed.iter().map(|(a, b)| (a.clone(), s(b))).collect())),
            ("files", Json::Arr(rows)),
        ]))
        .as_bytes(),
        false,
    )?;
    Ok(rekeyed)
}

/// `"services"`: each named service's `"Start"` in the site's prefix, set in
/// place in the bundle's `system.reg` (same length: eight hex digits), under
/// `CurrentControlSet` and `ControlSet001` wherever the key is. A name the
/// prefix has no key for, or a key with no `"Start"`, fails the build.
/// The services `"helpers": "strip"` disables: the ones Wine's prefix starts
/// with every session (or a game's first device or drive query) that no game
/// eject has needed. A config's own `"services"` entry for one wins.
const STRIP_SERVICES: &[&str] = &["PlugPlay", "NDIS", "nsiproxy", "MountMgr"];

/// The programs `"helpers": "strip"` leaves out of the bundle: ones Wine
/// starts on the way to the window that do nothing a page can use.
const STRIP_PROGRAMS: &[&str] = &["winemenubuilder.exe"];

/// `services`, each `(name, start, required)`: a required service the prefix
/// has no key for fails the build, another is passed over.
fn set_services(v: &Json, web: &Path, services: &[(String, u32, bool)]) -> Result<()> {
    let what = crate::project::PROJECT_FILE;
    let path = web.join("rootfs.tar");
    let mut bytes = std::fs::read(&path)?;
    let env = json::str_list(json::field(v, "page", what)?, "env", what)?;
    let prefix = env.iter().find_map(|e| e.strip_prefix("WINEPREFIX=")).unwrap_or("/pfx").trim_matches('/').to_string();
    let want = format!("{prefix}/system.reg");
    let (o, n) = {
        let tar = crate::reach::Tar::parse(&bytes)?;
        let found = tar.files().into_iter().find(|(p, _, _)| *p == want).map(|(_, o, n)| (o, n));
        found.ok_or_else(|| anyhow!("\"services\": the bundle has no {want}"))?
    };
    let reg = &mut bytes[o..o + n];
    let lower: Vec<u8> = reg.to_ascii_lowercase();
    let mut said = Vec::new();
    for (name, start, required) in services {
        let mut keys = 0;
        for set in ["currentcontrolset", "controlset001"] {
            let head = format!("[system\\\\{set}\\\\services\\\\{}]", name.to_ascii_lowercase());
            let Some(at) = find(&lower, head.as_bytes(), 0) else { continue };
            let end = find(&lower, b"\n[", at + head.len()).unwrap_or(lower.len());
            let tag = b"\"start\"=dword:";
            let Some(v) = find(&lower[..end], tag, at) else {
                bail!("\"services\": \"{name}\": its key in {want} has no \"Start\"");
            };
            let d = v + tag.len();
            reg[d..d + 8].copy_from_slice(format!("{start:08x}").as_bytes());
            keys += 1;
        }
        if keys == 0 {
            if !required {
                continue;
            }
            bail!("\"services\": the prefix has no service \"{name}\" ({want})");
        }
        said.push(format!("{name} {}", ["boot", "system", "auto", "demand", "disabled"][*start as usize]));
    }
    // A new file: the one there may be a hard link to the project's bundle.
    std::fs::remove_file(&path)?;
    write_file(&path, &bytes, false)?;
    eprintln!("gexc eject build: \"services\": {}", said.join(", "));
    Ok(())
}

/// `"desktop": "none"`: the display driver every process loads itself,
/// with no desktop process to name it (Wine's `WINEGEX_NO_DESKTOP`).
const NO_DESKTOP_DRIVER: &str = "winewayland.drv";

/// `system.reg`'s text without `names`' values in its `RunServices` key,
/// and the names it had.
fn run_services_without(text: &str, names: &[&str]) -> (String, Vec<String>) {
    let head = "[software\\\\microsoft\\\\windows\\\\currentversion\\\\runservices]";
    let lower = text.to_ascii_lowercase();
    let Some(at) = lower.find(head) else { return (text.to_string(), Vec::new()) };
    let end = lower[at + head.len()..].find("\n[").map_or(lower.len(), |e| at + head.len() + e);
    let mut out = String::with_capacity(text.len());
    let mut gone = Vec::new();
    let mut here = 0;
    for line in text.split_inclusive('\n') {
        let start = here;
        here += line.len();
        let value = line.to_ascii_lowercase();
        if start > at && start < end {
            if let Some(n) = names.iter().find(|n| value.starts_with(&format!("\"{}\"=", n.to_ascii_lowercase()))) {
                gone.push(n.to_string());
                continue;
            }
        }
        out.push_str(line);
    }
    (out, gone)
}

/// Takes `names` out of the prefix's `RunServices` (the programs
/// `wineboot --init` starts each session) in web/rootfs.tar's `system.reg`.
/// A name the key does not have is passed over. Returns the ones taken out.
fn drop_run_services(v: &Json, web: &Path, names: &[&str]) -> Result<Vec<String>> {
    let what = crate::project::PROJECT_FILE;
    let path = web.join("rootfs.tar");
    let bytes = std::fs::read(&path)?;
    let env = json::str_list(json::field(v, "page", what)?, "env", what)?;
    let prefix = env.iter().find_map(|e| e.strip_prefix("WINEPREFIX=")).unwrap_or("/pfx").trim_matches('/').to_string();
    let want = format!("{prefix}/system.reg");
    let text = {
        let tar = crate::reach::Tar::parse(&bytes)?;
        let reg = tar.read(&want).ok_or_else(|| anyhow!("the bundle has no {want}"))?;
        String::from_utf8_lossy(reg).into_owned()
    };
    let (out, gone) = run_services_without(&text, names);
    if gone.is_empty() {
        return Ok(gone);
    }
    let new = crate::reach::tar_put(&bytes, &want, out.as_bytes())?;
    // web/rootfs.tar may be a hard link to the project's bundle.
    std::fs::remove_file(&path)?;
    write_file(&path, &new, false)?;
    eprintln!("gexc eject build: RunServices without {}", gone.join(", "));
    Ok(gone)
}

/// The `.reg` text of the recording's volatile keys under `root` ("HKLM"
/// or "HKCU"), as ordinary keys: each value as `hex(TYPE):`, a key with no
/// values still named. `CurrentControlSet` is written as the registry's
/// link to it, `ControlSet001`, which a prefix file names.
fn volatile_reg(keys: &[(String, Json)], root: &str) -> String {
    let mut out = String::new();
    let mut names: Vec<&(String, Json)> = keys.iter().filter(|(k, _)| k.split('\\').next() == Some(root)).collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    for (k, vals) in names {
        let mut path = k[root.len()..].trim_start_matches('\\').to_string();
        if path.get(..25).is_some_and(|p| p.eq_ignore_ascii_case("System\\CurrentControlSet\\")) {
            path = format!("System\\ControlSet001\\{}", &path[25..]);
        }
        out.push_str(&format!("[{}] 0\n", path.replace('\\', "\\\\")));
        if let Json::Obj(vals) = vals {
            for (name, val) in vals {
                let Json::Arr(tv) = val else { continue };
                let (Some(Json::Num(t)), Some(Json::Str(hex))) = (tv.first(), tv.get(1)) else { continue };
                let Ok(t) = t.parse::<u32>() else { continue };
                let bytes: Vec<&str> = (0..hex.len() / 2).map(|i| &hex[2 * i..2 * i + 2]).collect();
                let name = if name.is_empty() {
                    "@".to_string()
                } else {
                    format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
                };
                out.push_str(&format!("{name}=hex({t:x}):{}\n", bytes.join(",")));
            }
        }
        out.push('\n');
    }
    out
}

/// `"wineboot": "skip"`: the recording's volatile keys written into the
/// bundle's prefix, HKLM's into system.reg and HKCU's into user.reg, so a
/// session booted without wineboot (Wine's `WINEGEX_NO_WINEBOOT`) has what
/// it would have made. Returns how many keys.
/// Files that are images Wine loads: one the recorded run opened and no
/// process loaded (a DLL Wine looked at and passed over, or one only wineboot
/// used) is left out of a title bundle.
fn is_image(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    [".dll", ".exe", ".drv", ".sys", ".ocx", ".cpl", ".acm", ".ax", ".msstyles"].iter().any(|e| name.ends_with(e))
}

fn base_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase()
}

/// What `"bundle": "title"` keeps: the files the run had read by its title
/// window, or (`after` > 0) by that many seconds after it, from the
/// recording's `files_read_at`; less the images no process loaded (an image
/// is loaded under any of its names, so by its file name), and wineboot when
/// the site boots without it. The images left out are counted. None: the
/// recording has no read set for this cut.
fn title_read_set(v: &Json, after: u32, wineboot_skip: bool) -> Result<Option<(Vec<Json>, usize)>> {
    let read: Vec<String> = if after > 0 {
        let title = match v.get("title_seconds") {
            Some(Json::Num(n)) => n.parse::<f64>().unwrap_or(0.0),
            _ => bail!("\"title_after\" needs the recording's title_seconds"),
        };
        match v.get("files_read_at") {
            Some(Json::Obj(at)) => at
                .iter()
                .filter(|(_, t)| matches!(t, Json::Num(n) if n.parse::<f64>().map_or(false, |t| t <= title + after as f64)))
                .map(|(p, _)| p.clone())
                .collect(),
            _ => bail!(
                "user/eject.json says \"title_after\": {after}, but the recording has no files_read_at \
                 (record with a tools/eject/record-wine32.mjs that has the read times)"
            ),
        }
    } else {
        match v.get("files_read_title") {
            Some(Json::Arr(a)) => a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect(),
            _ => return Ok(None),
        }
    };
    let loaded: Option<std::collections::HashSet<String>> = match v.get("images_loaded") {
        Some(Json::Obj(kv)) => Some(kv.iter().map(|(p, _)| base_name(p)).collect()),
        _ => None,
    };
    let mut left = 0;
    let kept = read
        .into_iter()
        .filter(|p| {
            let out = is_image(p)
                && (loaded.as_ref().is_some_and(|l| !l.contains(&base_name(p)))
                    || (wineboot_skip && base_name(p) == "wineboot.exe"));
            left += out as usize;
            !out
        })
        .map(|p| json::s(&p))
        .collect();
    Ok(Some((kept, left)))
}

fn put_volatile_keys(v: &Json, web: &Path) -> Result<usize> {
    let what = crate::project::PROJECT_FILE;
    let Some(Json::Obj(keys)) = v.get("volatile_keys") else {
        bail!(
            "user/eject.json says \"wineboot\": \"skip\", but the recording has no volatile keys: record the page \
             with WINEGEX_RECORD_HARDWARE=1 in its environment (crates/gex-eject/README.md, \"Wineboot\")"
        )
    };
    let path = web.join("rootfs.tar");
    let mut bytes = std::fs::read(&path)?;
    let env = json::str_list(json::field(v, "page", what)?, "env", what)?;
    let prefix = env.iter().find_map(|e| e.strip_prefix("WINEPREFIX=")).unwrap_or("/pfx").trim_matches('/').to_string();
    for (root, file) in [("HKLM", "system.reg"), ("HKCU", "user.reg")] {
        let add = volatile_reg(keys, root);
        if add.is_empty() {
            continue;
        }
        let want = format!("{prefix}/{file}");
        let mut text = {
            let tar = crate::reach::Tar::parse(&bytes)?;
            let reg = tar.read(&want).ok_or_else(|| anyhow!("the bundle has no {want}"))?;
            String::from_utf8_lossy(reg).into_owned()
        };
        if !text.ends_with("\n\n") {
            text.push('\n');
        }
        text.push_str(&add);
        bytes = crate::reach::tar_put(&bytes, &want, text.as_bytes())?;
    }
    // web/rootfs.tar may be a hard link to the project's bundle.
    std::fs::remove_file(&path)?;
    write_file(&path, &bytes, false)?;
    eprintln!("gexc eject build: {} volatile keys from the recording in the prefix", keys.len());
    Ok(keys.len())
}

/// A path's last component, either separator.
fn strip_dir(p: &str) -> &str {
    p.rsplit(['/', '\\']).next().unwrap_or(p)
}

/// Whether `p` (a bundle or guest path) is one of STRIP_PROGRAMS.
fn strip_program(p: &str) -> bool {
    STRIP_PROGRAMS.iter().any(|n| strip_dir(p).eq_ignore_ascii_case(n))
}

/// `"helpers": "strip"`: web/rootfs.tar without STRIP_PROGRAMS, wherever
/// the bundle has them (the prefix's system32 and Wine's own directories).
/// Returns the paths left out.
fn strip_programs(web: &Path) -> Result<Vec<String>> {
    let path = web.join("rootfs.tar");
    let bytes = std::fs::read(&path)?;
    let (out, gone) = crate::reach::tar_drop(&bytes, &strip_program)?;
    if gone.is_empty() {
        return Ok(gone);
    }
    // web/rootfs.tar may be a hard link to the project's bundle.
    std::fs::remove_file(&path)?;
    write_file(&path, &out, false)?;
    eprintln!("gexc eject build: \"helpers\": left out {}", gone.join(", "));
    Ok(gone)
}

/// The units' region entries, whole, each under its unit's key on the
/// site: the page looks them up by the key it computes for the image.
fn copy_regions(dir: &Path, web: &Path, site_key: &dyn Fn(&str) -> String) -> Result<()> {
    let Ok(rd) = std::fs::read_dir(dir.join("units/regions")) else { return Ok(()) };
    for e in rd.flatten() {
        let key = site_key(&e.file_name().to_string_lossy());
        for f in ["c.wasm", "c.entries"] {
            link_or_copy(&e.path().join(f), &web.join("units/regions").join(&key).join(f))?;
        }
    }
    Ok(())
}

/// An AOT-only build has no code but its units: code the recording ran
/// translated in a unit's image (its `uncovered`) that neither the unit nor
/// its region entries hold fails the build, named.
fn check_covered(v: &Json, dir: &Path) -> Result<()> {
    let mut entries: std::collections::HashMap<String, std::collections::HashSet<u64>> = Default::default();
    let mut has = |key: &str, pc: u64| {
        entries
            .entry(key.to_string())
            .or_insert_with(|| {
                [dir.join("units").join(key), dir.join("units/regions").join(key)]
                    .iter()
                    .flat_map(|d| std::fs::read(d.join("c.entries")).unwrap_or_default())
                    .collect::<Vec<u8>>()
                    .chunks_exact(8)
                    .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
                    .collect()
            })
            .contains(&pc)
    };
    let missing: Vec<String> = list(v, "uncovered")
        .iter()
        .filter_map(|u| {
            let pc = u.get("pc")?.as_str()?;
            let key = u.get("key").and_then(|k| k.as_str()).unwrap_or("");
            let at = u64::from_str_radix(pc.trim_start_matches("0x"), 16).ok();
            // Native Wine's return pad: the run leaves there rather than
            // running code (the shim's `dispatch_miss`), so it needs no entry.
            if at == Some(gex_kernel::win::tramp::NATIVE_RETURN_PAD) || at.is_some_and(|a| has(key, a)) {
                return None;
            }
            let path = u.get("path").and_then(|p| p.as_str()).unwrap_or("?");
            Some(format!("{pc} ({path})"))
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let shown = missing.iter().take(40).cloned().collect::<Vec<_>>().join(", ");
    bail!(
        "gexc eject build: the recording ran {} addresses no unit or region entry holds, and this project runs without translation: \
         {shown}{}. Build region entries for them (gexc cgen-image --entries) into the unit cache and eject again, \
         or keep the function JIT for them (user/eject.json \"cold\": \"jit\")",
        missing.len(),
        if missing.len() > 40 { ", ..." } else { "" }
    )
}

/// Pin, in the bundle's prefix's `dll-bases`, every unit's image the file
/// leaves to the loader, at the base the recording had it (its `base`): a
/// unit is keyed by its image and its base, and the loader may place an
/// unpinned image elsewhere in another run (another process's layout, the
/// order things map), which an AOT-only page has no unit for. Images that
/// cannot move (no relocations) are left out. Returns the file's guest path
/// when it wrote it: every process's loader reads it, and the recorded run
/// may not have (the file is new, or it had no pins), so a bundle split
/// keeps it in the bundle.
fn pin_bases(v: &Json, units: &[Json], web: &Path) -> Result<Option<String>> {
    let what = crate::project::PROJECT_FILE;
    let path = web.join("rootfs.tar");
    let bytes = std::fs::read(&path)?;
    let env = json::str_list(json::field(v, "page", what)?, "env", what)?;
    let prefix = env.iter().find_map(|e| e.strip_prefix("WINEPREFIX=")).unwrap_or("/pfx").trim_end_matches('/').to_string();
    let file = format!("{}/dll-bases", prefix.trim_start_matches('/'));
    let tar = crate::reach::Tar::parse(&bytes)?;
    let old = tar.read(&file).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
    // The loader's name for a prefix file: the drive's, not the device's.
    let on_drive = |p: &str| -> String {
        match p.to_ascii_lowercase().find("/dosdevices/c:/") {
            Some(i) => format!("{}/drive_c/{}", &p[..i], &p[i + "/dosdevices/c:/".len()..]),
            None => p.to_string(),
        }
    };
    let drive = |p: &str| on_drive(p).to_ascii_lowercase();
    let pinned: std::collections::HashSet<String> =
        old.lines().filter_map(|l| l.split_once(' ')).map(|(_, p)| drive(p.trim())).collect();
    let mut taken: Vec<(u64, u64, String)> = Vec::new();
    for l in old.lines() {
        let Some((a, p)) = l.split_once(' ') else { continue };
        let Ok(a) = u64::from_str_radix(a.trim_start_matches("0x"), 16) else { continue };
        let size = tar.read(p.trim()).and_then(|d| gex_ld::pe::parse(d).ok()).map_or(0x10000, |pe| pe.image_size as u64);
        taken.push((a, a + size, p.trim().to_string()));
    }
    let mut add = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for u in units {
        let (Some(p), Some(b)) = (u.get("path").and_then(|x| x.as_str()), u.get("base").and_then(|x| x.as_str())) else { continue };
        if p.starts_with('[') || pinned.contains(&drive(p)) || !seen.insert(drive(p)) {
            continue;
        }
        let Ok(base) = u64::from_str_radix(b.trim_start_matches("0x"), 16) else { continue };
        let Some(pe) = tar.read(p).and_then(|d| gex_ld::pe::parse(d).ok()) else { continue };
        if pe.relocs.is_empty() {
            continue;
        }
        let end = base + pe.image_size as u64;
        if let Some((_, _, q)) = taken.iter().find(|(a, e, _)| base < *e && *a < end) {
            eprintln!("gexc eject build: {p} at {base:#x} overlaps {q}'s pin: left unpinned");
            continue;
        }
        taken.push((base, end, p.to_string()));
        add.push(format!("{base:#x} {}", on_drive(p)));
    }
    if add.is_empty() {
        return Ok(None);
    }
    let mut text = old.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    for l in &add {
        text.push_str(l);
        text.push('\n');
    }
    let out = crate::reach::tar_put(&bytes, &file, text.as_bytes())?;
    // web/rootfs.tar may be a hard link to the project's bundle.
    let _ = std::fs::remove_file(&path);
    write_file(&path, &out, false)?;
    eprintln!("gexc eject build: pinned {} images in {file}: {}", add.len(), add.join(", "));
    Ok(Some(format!("/{file}")))
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

#[derive(Debug, Default)]
pub struct Wine32Report {
    pub features_off: Vec<String>,
    pub page_files: usize,
    pub units: usize,
}

/// A new 32-bit Wine project's `"cold"`: none. Every unit ships whole, in the site's
/// one units archive (web/eject-site.mjs), and nothing is fetched on a call.
/// A split (`"chunk:256"` and the other clusterings) is opt-in: it cuts the
/// download and costs a fetch per cluster at first call (crates/gex-eject/README.md,
/// "Cold code").
const COLD_DEFAULT: &str = "none";

/// `gexc eject build PROJECT` for a 32-bit Wine project.
pub fn build(dir: &Path) -> Result<Wine32Report> {
    let p = dir.join(crate::project::PROJECT_FILE);
    let v = Json::parse(&std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?)
        .map_err(|e| anyhow!("{}: {e}", p.display()))?;
    let what = crate::project::PROJECT_FILE;
    let wine64 = json::opt_str(&v, "kind").as_deref() == Some(KIND64);
    let (config, config_bytes) = Config::load(dir)?;
    crate::features::check(&config.features_off)?;
    let mut off = config.features_off.clone();
    let aot = aot_only(&config)?;
    if aot && !off.iter().any(|x| x == "translation") {
        off.push("translation".to_string());
    }
    if aot {
        check_covered(&v, dir)?;
    }

    let web = dir.join("build/web");
    if web.exists() {
        std::fs::remove_dir_all(&web).with_context(|| format!("removing {}", web.display()))?;
    }
    std::fs::create_dir_all(&web)?;

    // The page, built with the features kept: a feature off is code the
    // bundler leaves out (GEX_FEATURES_OFF, vite's define) or files not
    // shipped.
    let src = PathBuf::from(json::str_field(&v, "page_source", what)?);
    let page = web.join("page");
    let st = std::process::Command::new("sh")
        .arg(src.join("build-eject-page.sh"))
        .arg(&page)
        .env("GEX_FEATURES_OFF", off.join(","))
        .status()
        .with_context(|| format!("running {}/build-eject-page.sh", src.display()))?;
    if !st.success() {
        bail!("{}/build-eject-page.sh failed ({st})", src.display());
    }
    // The runtime is the project's, not whatever the checkout has built since.
    for f in runtime_wasm(wine64) {
        let to = page.join("gex").join(f);
        let _ = std::fs::remove_file(&to);
        link_or_copy(&dir.join("runtime/web/gex").join(f), &to)?;
    }
    // GL kept, but the checkout has no Mesa built (web/build-mesa-wasm.sh):
    // the page would answer the guest's first GL request with a device that
    // cannot start. The build is one without GL instead, which the guest
    // sees as no /dev/gex-gl at all.
    if !off.iter().any(|x| x == "gl") && !page.join("gex/gex-wgpu-replay.js").is_file() {
        eprintln!("gexc eject build: {} has no Mesa (web/build-mesa-wasm.sh): the page is built without GL", src.display());
        off.push("gl".to_string());
    }
    let has = |f: &str| !off.iter().any(|x| x == f);
    if !has("gl") {
        for e in std::fs::read_dir(page.join("gex"))? {
            let e = e?;
            if e.file_name().to_string_lossy().starts_with("gex-wgpu-replay") {
                std::fs::remove_file(e.path())?;
            }
        }
    }
    if !has("compositor") {
        // What the define should have taken out: say so if it did not.
        for e in std::fs::read_dir(page.join("assets"))? {
            let n = e?.file_name().to_string_lossy().to_string();
            if n.starts_with("compositor-") || n.starts_with("gpu-device-") {
                bail!("the page still has {n} with \"compositor\": false (an import the define does not guard)");
            }
        }
    }
    if !page.join("app.mjs").is_file() {
        bail!("{}/build-eject-page.sh made no app.mjs (an older gex checkout? the page is the app library now)", src.display());
    }
    crate::web::check_no_test_page(&page)?;
    let mut page_files = 0;
    let mut stack = vec![page.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                stack.push(e.path());
            } else {
                page_files += 1;
            }
        }
    }

    // The bundle and the units: as they are, or with `"cold"` each unit's
    // primary (the functions the run called, a stub for every other) and
    // its cold clusters, `cold/N.wasm` (web/eject-site.mjs packs them).
    let units = list(&v, "units");
    // `"resources": "trim"`: the bundle with the resources no lookup returns
    // zeroed, and each trimmed image's unit under the key the page computes
    // for the trimmed image.
    let rekeyed = if config.resources_trim {
        trim_resources(dir, &v, units, &web)?
    } else {
        link_or_copy(&dir.join("bundle/rootfs.tar"), &web.join("rootfs.tar"))?;
        std::collections::HashMap::new()
    };
    // `"helpers": "strip"`: the bundle without the helper programs, and the
    // units without theirs.
    let kept_units: Vec<Json>;
    let units = if config.helpers_strip {
        strip_programs(&web)?;
        kept_units = units
            .iter()
            .filter(|u| !u.get("path").and_then(|x| x.as_str()).is_some_and(strip_program))
            .cloned()
            .collect();
        &kept_units[..]
    } else {
        units
    };
    let pins = pin_bases(&v, units, &web)?;
    let mut services: Vec<(String, u32, bool)> = Vec::new();
    if config.helpers_strip {
        for n in STRIP_SERVICES {
            if !config.services.iter().any(|(k, _)| k.eq_ignore_ascii_case(n)) {
                services.push((n.to_string(), 4, false));
            }
        }
    }
    services.extend(config.services.iter().map(|(k, s)| (k.clone(), *s, true)));
    if !services.is_empty() {
        set_services(&v, &web, &services)?;
    }
    let mut run_off = Vec::new();
    if config.helpers_strip {
        run_off.push("winemenubuilder");
    }
    if config.desktop_none {
        run_off.push("desktop");
    }
    if !run_off.is_empty() {
        drop_run_services(&v, &web, &run_off)?;
    }
    if config.wineboot_skip {
        put_volatile_keys(&v, &web)?;
    }
    let site_key = |k: &str| rekeyed.get(k).cloned().unwrap_or_else(|| k.to_string());
    let instrument = config.cold.as_deref() == Some("instrument");
    // `user/select.json` (`gexc units select`): the slots to ship, in place
    // of the recording's coverage.
    let selected: Option<Vec<(String, Json)>> = match std::fs::read_to_string(dir.join("user/select.json")) {
        Ok(t) if config.cold.is_some() && !instrument => Some(
            gex_units::select::read_selection(&t)?.into_iter().map(|(k, r)| (k, Json::Str(r))).collect(),
        ),
        _ => None,
    };
    let coverage = match (&config.cold, v.get("coverage")) {
        (None, _) => None,
        _ if selected.is_some() => selected.as_ref(),
        _ if instrument => None,
        (Some(_), Some(Json::Obj(kv))) => Some(kv),
        (Some(_), _) => {
            eprintln!(
                "gexc eject build: user/eject.json says \"cold\", but the recording has no coverage: \
                 the units stay whole (crates/gex-eject/README.md, \"Cold code\": record a site built with \
                 \"cold\": \"instrument\" with record-wine32.mjs --coverage-into)"
            );
            None
        }
    };
    let mut cold_units = 0;
    let mut jit_units = 0;
    // `"prune"`: each unit without the functions the proof leaves out
    // ("fanout": crate::fanout, across all the images together; "static":
    // crate::reach, one image at a time), in parallel.
    let mut pruned: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    if let Some(mode) = config.prune.as_deref() {
        if config.cold.is_some() {
            bail!("user/eject.json: \"prune\": \"{mode}\" goes with \"cold\": \"none\"");
        }
        let tar_bytes = std::fs::read(dir.join("bundle/rootfs.tar"))?;
        let tar = crate::reach::Tar::parse(&tar_bytes)?;
        let jobs: Vec<(String, String)> = units
            .iter()
            .map(|u| Ok((json::str_field(u, "key", what)?, json::str_field(u, "path", what)?)))
            .collect::<Result<_>>()?;
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
        let mut rows = Vec::new();
        let (mut before, mut after, mut slots_all, mut kept_all) = (0usize, 0usize, 0usize, 0usize);
        let mut extra = String::new();
        if mode == "fanout" {
            let mut work: Vec<(String, String, Vec<u8>, Vec<u8>)> = Vec::new();
            for (key, path) in &jobs {
                let wasm = std::fs::read(dir.join("units").join(key).join("c.wasm"))?;
                let image = tar.read(path).ok_or_else(|| anyhow!("{path}: not in bundle/rootfs.tar"))?.to_vec();
                work.push((key.clone(), path.clone(), wasm, image));
            }
            let native = crate::fanout::native_names(&v, &tar)?;
            let (out, keeps) = crate::fanout::run(work, &native, threads)?;
            for p in out {
                before += p.bytes_before;
                after += p.bytes_after;
                slots_all += p.kept.slots;
                kept_all += p.kept.kept;
                rows.push(obj(vec![
                    ("key", s(&p.key)),
                    ("path", s(&p.path)),
                    ("slots", json::n(p.kept.slots as u64)),
                    ("kept", json::n(p.kept.kept as u64)),
                    ("functions", json::n(p.kept.functions as u64)),
                    ("exports", json::n(p.kept.exports as u64)),
                    ("exports_kept", json::n(p.kept.exports_kept as u64)),
                    ("bytes_before", json::n(p.bytes_before as u64)),
                    ("bytes_after", json::n(p.bytes_after as u64)),
                    ("whole", p.kept.whole.as_deref().map(s).unwrap_or(Json::Null)),
                ]));
                pruned.insert(p.key, p.wasm);
            }
            extra = crate::fanout::keeps_json(&keeps);
            eprint!("{}", crate::fanout::keeps_text(&keeps));
        } else {
            let next = std::sync::atomic::AtomicUsize::new(0);
            let results: Vec<Result<(Vec<u8>, crate::reach::PruneStat)>> = {
                let slots: Vec<std::sync::Mutex<Option<Result<(Vec<u8>, crate::reach::PruneStat)>>>> =
                    jobs.iter().map(|_| std::sync::Mutex::new(None)).collect();
                std::thread::scope(|sc| {
                    for _ in 0..threads {
                        sc.spawn(|| loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some((key, path)) = jobs.get(i) else { break };
                            let r = (|| {
                                let wasm = std::fs::read(dir.join("units").join(key).join("c.wasm"))?;
                                let image = tar.read(path).ok_or_else(|| anyhow!("{path}: not in bundle/rootfs.tar"))?;
                                crate::reach::prune_unit(&wasm, image, key, path)
                            })();
                            *slots[i].lock().unwrap() = Some(r);
                        });
                    }
                });
                slots.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
            };
            for r in results {
                let (wasm, st) = r?;
                before += st.bytes_before;
                after += st.bytes_after;
                slots_all += st.slots;
                kept_all += st.kept;
                rows.push(obj(vec![
                    ("key", s(&st.key)),
                    ("path", s(&st.path)),
                    ("slots", json::n(st.slots as u64)),
                    ("kept", json::n(st.kept as u64)),
                    ("bytes_before", json::n(st.bytes_before as u64)),
                    ("bytes_after", json::n(st.bytes_after as u64)),
                    ("whole", st.whole.as_deref().map(s).unwrap_or(Json::Null)),
                ]));
                pruned.insert(st.key, wasm);
            }
        }
        eprintln!(
            "gexc eject build: \"prune\": \"{mode}\" left out {} of {slots_all} functions; units {before} -> {after} bytes",
            slots_all - kept_all
        );
        let mut fields = vec![
            ("mode", s(mode)),
            ("slots", json::n(slots_all as u64)),
            ("kept", json::n(kept_all as u64)),
            ("bytes_before", json::n(before as u64)),
            ("bytes_after", json::n(after as u64)),
        ];
        if !extra.is_empty() {
            fields.push(("keeps", Json::parse(&extra).map_err(|e| anyhow!("the prune report: {e}"))?));
        }
        fields.push(("units", Json::Arr(rows)));
        write_file(&dir.join("build/prune.json"), json::write(&obj(fields)).as_bytes(), false)?;
    }
    for u in units {
        let key = json::str_field(u, "key", what)?;
        let from = dir.join("units").join(&key);
        let to = web.join("units").join(site_key(&key));
        let jit = coverage.is_some() && matches!(config.cold.as_deref(), Some("jit" | "tier1"));
        let hot;
        let shape = if instrument {
            gex_units::Shape::Instrument
        } else if let Some(w) = pruned.get(&key) {
            gex_units::Shape::Pruned(w)
        } else if let (Some(cov), Some(how)) = (coverage, config.cold.as_deref()) {
            hot = match cov.iter().find(|(k, _)| *k == key).map(|(_, r)| r) {
                Some(Json::Str(r)) => gex_units::slot_ranges(r).with_context(|| format!("the recording's coverage of {key}"))?,
                _ => std::collections::HashSet::new(),
            };
            if jit {
                gex_units::Shape::Jit(&hot)
            } else {
                gex_units::Shape::Split(&hot, how.parse()?)
            }
        } else {
            gex_units::Shape::Whole
        };
        let what_unit = if jit { format!("unit {key}") } else { format!("splitting unit {key}") };
        let shipped = gex_units::ship(&from, &to, shape).with_context(|| what_unit)?;
        jit_units += shipped.jit as usize;
        cold_units += shipped.cold as usize;
    }

    // The provider modules the units bind, beside them as in a unit cache.
    if let Ok(rd) = std::fs::read_dir(dir.join("units/providers")) {
        for e in rd.flatten() {
            link_or_copy(&e.path(), &web.join("units/providers").join(e.file_name()))?;
        }
    }
    copy_regions(dir, &web, &site_key)?;

    // How the page starts the program: the recording's, less what the site
    // decides (where the bundle and the units are, what to preload).
    let rec = json::field(&v, "page", what)?;
    let params: Vec<(String, Json)> = match rec.get("params") {
        Some(Json::Obj(kv)) => kv
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "tar" | "cgenurl" | "preload" | "exe" | "argv" | "args" | "env" | "cwd" | "mtimes" | "size" | "compositor" | "cold"))
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    // Split units: the page fetches their cold clusters (gex.mjs `gex.cold`).
    let mut params = params;
    // Translation off: nothing is translated at run time (app.js `aot`).
    params.retain(|(k, _)| k != "aot");
    if aot {
        params.push(("aot".to_string(), s("1")));
    }
    // Each unit's image key and identity as the page computes them for the
    // image it ships (cgen.rs `known`), which the page takes instead of
    // hashing every byte of each image it maps.
    let identities = unit_identities(dir, &web, units, &site_key)?;
    if !identities.is_empty() {
        params.retain(|(k, _)| k != "identities");
        params.push(("identities".to_string(), s(&identities)));
    }
    // user/eject.json "providers": stateful providers in place of those
    // libraries, built from their sources into the units the page loads
    // (cgen.rs `set_stateful`).
    if !config.providers.is_empty() {
        let gen = dir.join("build/gen");
        write_file(&gen.join("provider.h"), crate::provider::PROVIDER_H.as_bytes(), false)?;
        write_file(&gen.join("gex_abi.h"), gex_cgen::header::header().as_bytes(), false)?;
        let mut spec = Vec::new();
        for p in &config.providers {
            let rel = format!("stateful/{}", p.library.to_ascii_lowercase().replace(".dll", ".wasm"));
            crate::provider::build_stateful(dir, p, &web.join("units").join(&rel), &gen)?;
            spec.push(format!("{}={rel}", p.library.to_ascii_lowercase()));
        }
        params.retain(|(k, _)| k != "stateful");
        params.push(("stateful".to_string(), s(&spec.join(","))));
    }
    if cold_units > 0 {
        params.push(("cold".to_string(), s("1")));
    }
    // `"cold": "jit"`: what the units leave out is the function JIT's.
    // `"tier1"`: the same units, and tier 1 for the rest.
    if jit_units > 0 && config.cold.as_deref() == Some("jit") {
        params.retain(|(k, _)| k != "fjit");
        params.push(("fjit".to_string(), s("1")));
    }
    // `"bundle": "split"`: the files the recorded run opened, which the site
    // keeps in its bundle (web/eject-site.mjs); `"title"`: those it opened up
    // to its title window.
    let read_set = if config.bundle_title { "files_read_title" } else { "files_read" };
    let bundle = if config.bundle_split {
        let read = if config.bundle_title {
            title_read_set(&v, config.title_after, config.wineboot_skip)?.map(|(read, left)| {
                eprintln!(
                    "gexc eject build: \"bundle\": \"title\"{}: {} files in the bundle, {left} images the run only opened left to be read later",
                    if config.title_after > 0 { format!(" {} s after the window", config.title_after) } else { String::new() },
                    read.len()
                );
                read
            })
        } else {
            match v.get(read_set) {
                Some(Json::Arr(read)) => Some(read.clone()),
                _ => None,
            }
        };
        match read {
            Some(read) => {
                let mut read = read;
                if let Some(p) = pins.filter(|p| !read.iter().any(|r| r.as_str() == Some(p))) {
                    read.push(s(&p));
                }
                obj(vec![("split", json::b(true)), ("read", Json::Arr(read))])
            }
            _ => bail!(
                "user/eject.json says \"bundle\": \"{}\", but the recording has no {read_set} \
                 (record with a tools/eject/record-wine32.mjs and page that have the read set, or set \"bundle\": \"whole\")",
                if config.bundle_title { "title" } else { "split" }
            ),
        }
    } else {
        obj(vec![("split", json::b(false))])
    };
    // The bundle's extended attributes (the memory64 mark on a program,
    // Wine's DOS attributes): the site's bundle is rebuilt from a tree that
    // does not keep them, so the launch file carries them to the page.
    let xattrs = {
        let bytes = std::fs::read(web.join("rootfs.tar"))?;
        crate::reach::xattrs(&bytes)?
    };
    let launch = obj(vec![
        ("kind", s(if wine64 { KIND64 } else { KIND })),
        ("d3d", rec.get("d3d").cloned().unwrap_or(json::b(false))),
        ("xattrs", Json::Obj(xattrs.into_iter().map(|(p, a)| (p, Json::Obj(a.into_iter().map(|(n, v)| (n, s(&v))).collect()))).collect())),
        ("exe", rec.get("exe").cloned().unwrap_or(Json::Null)),
        ("argv", rec.get("argv").cloned().unwrap_or(Json::Arr(Vec::new()))),
        ("env", {
            let mut env: Vec<Json> = match rec.get("env") {
                Some(Json::Arr(a)) => a.clone(),
                _ => Vec::new(),
            };
            // `"desktop": "none"`: no desktop process; each loads the driver.
            if config.desktop_none {
                env.retain(|e| !e.as_str().is_some_and(|x| x.starts_with("WINEGEX_NO_DESKTOP=")));
                env.push(s(format!("WINEGEX_NO_DESKTOP={NO_DESKTOP_DRIVER}")));
            }
            // `"wineboot": "skip"`: the session boots without it (and the
            // recording's ask for its keys, there being none to make).
            if config.wineboot_skip {
                env.retain(|e| !e.as_str().is_some_and(|x| x.starts_with("WINEGEX_NO_WINEBOOT=") || x.starts_with("WINEGEX_RECORD_HARDWARE=")));
                env.push(s("WINEGEX_NO_WINEBOOT=1"));
            }
            Json::Arr(env)
        }),
        ("cwd", rec.get("cwd").cloned().unwrap_or(s("/"))),
        ("mtimes", rec.get("mtimes").cloned().unwrap_or(json::b(false))),
        // The size the recording asked for ("fit": the page's), not the one
        // it resolved to (the recorder's window): a fixed size is the
        // recorder's viewport on every screen.
        ("size", match rec.get("params").and_then(|p| p.get("size")) {
            Some(Json::Str(z)) if !z.is_empty() => s(z),
            _ => rec.get("size").cloned().unwrap_or(s("fit")),
        }),
        ("title", v.get("title").cloned().unwrap_or(Json::Null)),
        ("title_seconds", v.get("title_seconds").cloned().unwrap_or(Json::Null)),
        ("params", Json::Obj(params)),
        ("features_off", strs(&off)),
        ("units", Json::Arr(units.iter().map(|u| {
            let k = json::opt_str(u, "key").unwrap_or_default();
            let mut u = u.clone();
            if let Json::Obj(kv) = &mut u {
                for (_, x) in kv.iter_mut().filter(|(f, _)| f == "key") {
                    *x = s(&site_key(&k));
                }
            }
            u
        }).collect())),
        ("bundle", bundle),
        // user/eject.json "start": the app library's start() defaults, over
        // the recording's (crates/gex-eject/README.md, "The web build").
        ("start", Json::parse(&config.start).map_err(|e| anyhow!("user/eject.json \"start\": {e}"))?),
    ]);
    write_file(&web.join("wine32.json"), json::write(&launch).as_bytes(), false)?;
    write_file(
        &dir.join("build/build.json"),
        json::write(&obj(vec![
            ("kind", s(if wine64 { KIND64 } else { KIND })),
            ("features_off", strs(&off)),
            ("config", s(sha256(&config_bytes))),
            ("session", v.get("session").cloned().unwrap_or(Json::Null)),
            ("page_files", json::n(page_files as u64)),
            ("units", json::n(units.len() as u64)),
            ("bundle_split", json::b(config.bundle_split)),
            ("cold", config.cold.as_deref().map(s).unwrap_or(Json::Null)),
            ("prune", s(config.prune.as_deref().unwrap_or("none"))),
        ]))
        .as_bytes(),
        false,
    )?;
    Ok(Wine32Report { features_off: off, page_files, units: units.len() })
}

#[cfg(test)]
mod volatile_tests {
    use super::*;

    #[test]
    fn a_title_bundle_leaves_out_what_no_process_loaded() {
        let v = Json::parse(
            r#"{"title_seconds": 40.5,
                "files_read_title": ["/w/user32.dll", "/w/gdiplus.dll", "/w/sortdefault.nls", "/w/wineboot.exe", "/w/winepe.wasm"],
                "files_read_at": {"/w/user32.dll": 30, "/w/gdiplus.dll": 31, "/w/sortdefault.nls": 32, "/w/wineboot.exe": 10,
                                  "/w/winepe.wasm": 10, "/g/menu.pkg": 70, "/g/level.pkg": 200},
                "images_loaded": {"/pfx/c/windows/system32/USER32.dll": 30, "/w/wineboot.exe": 10}}"#,
        )
        .unwrap();
        let names = |r: Option<(Vec<Json>, usize)>| {
            let (r, left) = r.unwrap();
            let mut r: Vec<String> = r.iter().map(|p| p.as_str().unwrap().to_string()).collect();
            r.sort();
            (r, left)
        };
        assert_eq!(names(title_read_set(&v, 0, false).unwrap()), (vec!["/w/sortdefault.nls".into(), "/w/user32.dll".into(), "/w/wineboot.exe".into(), "/w/winepe.wasm".into()], 1));
        assert_eq!(names(title_read_set(&v, 0, true).unwrap()).1, 2);
        let (later, _) = names(title_read_set(&v, 40, true).unwrap());
        assert!(later.contains(&"/g/menu.pkg".to_string()) && !later.contains(&"/g/level.pkg".to_string()));
        assert!(title_read_set(&Json::parse("{}").unwrap(), 0, false).unwrap().is_none());
        assert!(title_read_set(&Json::parse(r#"{"title_seconds": 1}"#).unwrap(), 5, false).is_err());
    }

    #[test]
    fn writes_recorded_keys_as_ordinary_ones() {
        let v = Json::parse(
            r#"{"volatile_keys": {
                "HKLM\\Hardware\\Description\\System\\CentralProcessor": {},
                "HKLM\\System\\CurrentControlSet\\Control\\ComputerName\\ActiveComputerName": {"ComputerName": [1, "47000000"]},
                "HKCU\\Volatile Environment": {"USERNAME": [1, "670000"]}}}"#,
        )
        .unwrap();
        let Some(Json::Obj(keys)) = v.get("volatile_keys") else { panic!() };
        let m = volatile_reg(keys, "HKLM");
        assert!(m.contains("[Hardware\\\\Description\\\\System\\\\CentralProcessor] 0\n\n"));
        assert!(m.contains("[System\\\\ControlSet001\\\\Control\\\\ComputerName\\\\ActiveComputerName] 0\n\"ComputerName\"=hex(1):47,00,00,00\n"));
        assert!(!m.contains("Volatile"));
        assert_eq!(volatile_reg(keys, "HKCU"), "[Volatile Environment] 0\n\"USERNAME\"=hex(1):67,00,00\n\n");
    }
}

#[cfg(test)]
mod run_services_tests {
    use super::*;

    #[test]
    fn takes_values_out_of_run_services_only() {
        let reg = "[Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\RunServices] 1\n#time=1\n\"winemenubuilder\"=\"a\"\n\"desktop\"=\"b\"\n\n[Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\RunServicesOnce] 1\n\"desktop\"=\"c\"\n";
        let (out, gone) = run_services_without(reg, &["desktop", "absent"]);
        assert_eq!(gone, vec!["desktop".to_string()]);
        assert!(out.contains("\"winemenubuilder\"=\"a\"\n\n"));
        assert!(out.ends_with("RunServicesOnce] 1\n\"desktop\"=\"c\"\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests that set the process's KUSER fold, one at a time.
    static FOLD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn config(features: &str, cold: Option<&str>) -> Config {
        let dir = std::env::temp_dir().join(format!("gex-eject-wine32-aot-{}-{}", std::process::id(), features.len() + cold.map_or(0, |c| c.len() * 7)));
        std::fs::create_dir_all(dir.join("user")).unwrap();
        let cold = cold.map(|c| format!(", \"cold\": \"{c}\"")).unwrap_or_default();
        std::fs::write(dir.join(crate::config::CONFIG_FILE), format!("{{\"features\": {{{features}}}{cold}}}")).unwrap();
        let c = Config::load(&dir).unwrap().0;
        std::fs::remove_dir_all(&dir).unwrap();
        c
    }

    #[test]
    fn an_aot_only_build_refuses_code_no_region_entry_holds() {
        let dir = std::env::temp_dir().join(format!("gex-wine32-covered-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ok = Json::parse(r#"{"uncovered": []}"#).unwrap();
        assert!(check_covered(&ok, &dir).is_ok());
        // Native Wine's return pad is never code to cover.
        let pad = format!(r#"{{"uncovered": [{{"pc": "{:#x}", "key": "t", "path": "[the trampolines]"}}]}}"#, gex_kernel::win::tramp::NATIVE_RETURN_PAD);
        assert!(check_covered(&Json::parse(&pad).unwrap(), &dir).is_ok());
        let bad = Json::parse(r#"{"uncovered": [{"pc": "0x612c52cd", "key": "k", "path": "/wine32/combase.dll"}]}"#).unwrap();
        let e = check_covered(&bad, &dir).unwrap_err().to_string();
        assert!(e.contains("0x612c52cd (/wine32/combase.dll)"), "{e}");
        // A region entry of its unit's holds it.
        std::fs::create_dir_all(dir.join("units/regions/k")).unwrap();
        let entries: Vec<u8> = [0x612c0000u64, 0x612c52cd].iter().flat_map(|a| a.to_le_bytes()).collect();
        std::fs::write(dir.join("units/regions/k/c.entries"), entries).unwrap();
        assert!(check_covered(&bad, &dir).is_ok());
        // Another unit's does not.
        let other = Json::parse(r#"{"uncovered": [{"pc": "0x612c52cd", "key": "j", "path": "/wine32/ole32.dll"}]}"#).unwrap();
        assert!(check_covered(&other, &dir).is_err());
        // Its unit's own entries hold it too (a later recovery found it).
        std::fs::create_dir_all(dir.join("units/j")).unwrap();
        std::fs::write(dir.join("units/j/c.entries"), 0x612c52cdu64.to_le_bytes()).unwrap();
        assert!(check_covered(&other, &dir).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn region_entries_follow_their_units_new_key() {
        let dir = std::env::temp_dir().join(format!("gex-wine32-regions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for k in ["old", "kept"] {
            std::fs::create_dir_all(dir.join("units/regions").join(k)).unwrap();
            std::fs::write(dir.join("units/regions").join(k).join("c.wasm"), k).unwrap();
            std::fs::write(dir.join("units/regions").join(k).join("c.entries"), k).unwrap();
        }
        let web = dir.join("web");
        copy_regions(&dir, &web, &|k: &str| if k == "old" { "new".into() } else { k.to_string() }).unwrap();
        assert_eq!(std::fs::read_to_string(web.join("units/regions/new/c.wasm")).unwrap(), "old");
        assert!(web.join("units/regions/kept/c.entries").is_file());
        assert!(!web.join("units/regions/old").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The trampolines' provider is an x86 guest's (`--features x86`, as gexc's).
    #[cfg(feature = "x86")]
    #[test]
    fn provider_files_name_the_wine32_kuser_build() {
        use gex_cgen::provider;
        let _fold = FOLD.lock().unwrap_or_else(|e| e.into_inner());
        let was = gex_abi::kuser_fold();
        let ps = provider::for_dll(provider::TRAMPOLINES_DLL, true);
        let p = ps.iter().find(|p| !p.binds.is_empty()).expect("the trampolines have a provider");
        let fp = |on: bool| {
            gex_abi::set_kuser_fold(on);
            let f = provider::fingerprint(std::slice::from_ref(p));
            gex_abi::set_kuser_fold(was);
            f
        };
        let (on, off) = (fp(true), fp(false));
        assert_ne!(on, off, "the fold changes the provider's header");
        // The fold-on name (what `cgen-image --wine32-kuser --providers` builds and
        // the page asks for), whatever the caller's fold, which is put back.
        for caller in [false, true] {
            gex_abi::set_kuser_fold(caller);
            assert_eq!(provider_file(p), format!("providers/{}-{on}.wasm", p.name));
            assert_eq!(gex_abi::kuser_fold(), caller);
        }
        gex_abi::set_kuser_fold(was);
    }

    #[test]
    fn a_wine32_eject_ships_the_wine32_kuser_trampoline_unit() {
        let _fold = FOLD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("gex-wine32-tramp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let was = gex_abi::kuser_fold();
        let keys = trampoline_keys();
        // The fold-on keys, not this process's flavor: a gexc eject runs with the fold off.
        let (image, _) = gex_kernel::win::trampoline_input(true);
        assert!(!keys.contains(&crate::rsrc::key_with(&image, (false, false))));
        assert!(keys.iter().all(|k| k.len() == 64));
        assert_eq!(gex_abi::kuser_fold(), was);
        let mut units = vec![("a".repeat(64), "/wine32/lib/wine/i386-windows/ntdll.dll".to_string())];
        // None in the cache: the eject fails, naming the build.
        let e = add_trampolines(&mut units, &[dir.clone()]).unwrap_err().to_string();
        assert!(e.contains("--wine32-kuser --nt-trampolines32"), "{e}");
        // One in the cache: added once, by its key.
        std::fs::create_dir_all(dir.join(&keys[0])).unwrap();
        for f in ["c.wasm", "c.entries"] {
            std::fs::write(dir.join(&keys[0]).join(f), f).unwrap();
        }
        add_trampolines(&mut units, &[dir.clone()]).unwrap();
        add_trampolines(&mut units, &[dir.clone()]).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[1], (keys[0].clone(), TRAMPOLINES_PATH.to_string()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_trampoline_unit_is_packed_with_the_boot() {
        // eject-site.mjs compiles with the page (and counts in its boot
        // archive) a unit whose `at` is at most the title's seconds; one
        // with no `at` is left for the process that maps it, and none maps
        // the trampolines' unit.
        let recorded = Json::parse(r#"[{"key":"aa","path":"/x/a.dll","at":3.5,"base":"0x10000000"},{"key":"bb","path":"/x/b.dll"}]"#).unwrap();
        let recorded = recorded.as_arr().unwrap();
        let units = vec![
            ("aa".to_string(), "/x/a.dll".to_string()),
            ("bb".to_string(), "/x/b.dll".to_string()),
            ("cc".to_string(), TRAMPOLINES_PATH.to_string()),
        ];
        let j = units_json(recorded, &units);
        let us = j.as_arr().unwrap();
        let at = |i: usize| us[i].get("at").cloned();
        assert_eq!(at(0), Some(json::Json::Num("3.5".into())));
        assert_eq!(us[0].get("base").cloned(), Some(s("0x10000000")));
        assert_eq!(at(1), None);
        assert_eq!(at(2), Some(json::n(0)));
        assert_eq!(json::opt_str(&us[2], "path").as_deref(), Some(TRAMPOLINES_PATH));
        // A session that recorded it keeps its own `at`.
        let recorded2 = Json::parse(r#"[{"key":"cc","path":"[nt trampolines]","at":1.25}]"#).unwrap();
        let j2 = units_json(recorded2.as_arr().unwrap(), &units[2..]);
        assert_eq!(j2.as_arr().unwrap()[0].get("at").cloned(), Some(json::Json::Num("1.25".into())));
    }

    #[test]
    fn a_wine64_eject_ships_the_compact_trampoline_unit() {
        let _fold = FOLD.lock().unwrap_or_else(|e| e.into_inner());
        let (image, _) = gex_kernel::win::trampoline_input(false);
        let keys = trampoline_keys64();
        // `--wine64-compact`'s keys (KUSER fold and entry page), guarded or not,
        // beside the plain and memory64 ones, and none of them 32-bit Wine's fold alone.
        for unguarded in [false, true] {
            let compact = crate::rsrc::key_with_entry(&image, (unguarded, true), true);
            assert!(keys.contains(&compact), "{compact} not in {keys:?}");
            assert!(!keys.contains(&crate::rsrc::key_with(&image, (unguarded, true))));
        }
        assert_eq!(keys.len(), 6);
    }

    #[test]
    fn translation_is_off_unless_kept() {
        assert!(aot_only(&config("", None)).unwrap());
        assert!(aot_only(&config("\"translation\": false", None)).unwrap());
        assert!(!aot_only(&config("\"translation\": true", None)).unwrap());
        // The function JIT's cold code keeps a translator, and contradicts an explicit off.
        assert!(!aot_only(&config("", Some("jit"))).unwrap());
        assert!(aot_only(&config("\"translation\": false", Some("jit"))).is_err());
    }
}
