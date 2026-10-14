//! Ejecting a W3 page: a Windows program on Wine as native wasm32, recorded
//! in the browser (docs/eject.md, "W3 pages").
//!
//! A W3 program is not one process on the NT layer (`pe.rs`) but Wine: the
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
//!   session/            w3-session.json, the recording             generated
//!   bundle/rootfs.tar   the filesystem the page fetched            generated
//!   units/KEY/          the units the recording used               generated
//!   runtime/web/gex/    the browser shim, kernel and seed          generated
//!   build.sh            the build step                             generated
//!   user/eject.json     yours: features, ...                       created once
//!   build/web/          the site's inputs (build.sh): page/, rootfs.tar,
//!                       units/, w3.json
//! ```
//!
//! Recording: `tools/eject/record-w3.mjs` (headless Chromium) runs a W3 page
//! to a window title and writes `w3-session.json`. The page is built by the
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
pub const SESSION: &str = "w3-session.json";
/// What makes a project a W3 one.
pub const KIND: &str = "w3";

/// The runtime's wasm the page loads from `gex/` (web/build-eject-page.sh).
const RUNTIME_WASM: &[&str] = &["gex_browser.x86.shared.wasm", "gex_kernel.x86.wasm", "seed.x86.shared.wasm"];

/// Whether `record` holds a W3 recording.
pub fn is_recording(record: &Path) -> bool {
    record.join(SESSION).is_file()
}

/// Whether `dir` is a W3 project.
pub fn is_project(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(crate::project::PROJECT_FILE))
        .ok()
        .and_then(|t| Json::parse(&t).ok())
        .is_some_and(|v| v.get("kind").and_then(|k| k.as_str()) == Some(KIND))
}

pub struct W3Options {
    /// The gex checkout's `web/` directory: the page's source and
    /// `build-eject-page.sh`, and the built runtime under `public/gex/`.
    pub web_src: PathBuf,
    /// Where the units the recording names are (a unit cache: KEY/c.wasm).
    pub unit_cache: Vec<PathBuf>,
    /// The runtime's features a new project's `user/eject.json` switches off.
    pub features_off: Vec<String>,
}

#[derive(Debug, Default)]
pub struct W3Summary {
    pub processes: usize,
    pub units: usize,
    pub unit_bytes: u64,
    pub bundle_bytes: u64,
    pub modules: usize,
    pub kept_user: bool,
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

/// `gexc eject RECORD --out PROJECT` for a W3 recording.
pub fn eject(record: &Path, out: &Path, runtime: &Path, o: &W3Options) -> Result<W3Summary> {
    let session_path = record.join(SESSION);
    let text = std::fs::read_to_string(&session_path).with_context(|| format!("reading {}", session_path.display()))?;
    let v = Json::parse(&text).map_err(|e| anyhow!("{}: {e}", session_path.display()))?;
    let what = SESSION;
    if json::opt_str(&v, "format").as_deref() != Some("gex-w3-session") {
        bail!("{what}: not a W3 session (format \"gex-w3-session\")");
    }
    let page = json::field(&v, "page", what)?;
    json::str_field(page, "exe", what)?;
    let bundle = json::opt_str(&v, "bundle").ok_or_else(|| {
        anyhow!("{what}: no \"bundle\": record with --bundle PATH, the tar the page fetched")
    })?;
    let units: Vec<(String, String)> = list(&v, "units")
        .iter()
        .map(|u| Ok((json::str_field(u, "key", what)?, json::str_field(u, "path", what)?)))
        .collect::<Result<_>>()?;
    if units.iter().any(|(k, _)| k.len() != 64 || !k.bytes().all(|c| c.is_ascii_hexdigit())) {
        bail!("{what}: a unit key that is not 64 hex digits");
    }
    for f in RUNTIME_WASM {
        if !o.web_src.join("public/gex").join(f).is_file() {
            bail!("{}: no public/gex/{f}: build the shim and kernel first (shims/browser/build.sh, shims/kernel/build.sh)", o.web_src.display());
        }
    }
    if !o.web_src.join("build-eject-page.sh").is_file() {
        bail!("{} is not gex's web/ directory (no build-eject-page.sh)", o.web_src.display());
    }
    crate::features::check(&o.features_off)?;

    let kept_user = prepare(out)?;
    let mut sum = W3Summary { kept_user, ..W3Summary::default() };

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
    }
    sum.units = units.len();
    // The runtime: the browser shim, kernel and seed this page runs, and the
    // gexc that builds it.
    for f in RUNTIME_WASM {
        link_or_copy(&o.web_src.join("public/gex").join(f), &out.join("runtime/web/gex").join(f))?;
    }
    link_or_copy(runtime, &out.join("runtime/gexc"))?;

    let processes = list(&v, "processes");
    let modules = list(&v, "modules");
    sum.processes = processes.len();
    sum.modules = modules.len();
    let web_src = std::fs::canonicalize(&o.web_src).unwrap_or_else(|_| o.web_src.clone());
    let project = obj(vec![
        ("kind", s(KIND)),
        ("about", s("A W3 page (Wine as native wasm32) ejected by gexc eject; see docs/eject.md, \"W3 pages\". Generated: rewritten by every eject.")),
        ("page", page.clone()),
        ("title", v.get("title").cloned().unwrap_or(Json::Null)),
        ("bundle", obj(vec![("file", s("bundle/rootfs.tar")), ("sha256", s(&bundle_sha)), ("from", s(&bundle))])),
        ("processes", Json::Arr(processes.to_vec())),
        ("units", Json::Arr(units.iter().map(|(k, p)| obj(vec![("key", s(k)), ("path", s(p))])).collect())),
        ("unit_misses", Json::Arr(list(&v, "unit_misses").to_vec())),
        ("modules", Json::Arr(modules.to_vec())),
        // The files the run opened (the recorder's `files_read`; null from an
        // older recorder): what `"bundle": "split"` keeps in the bundle.
        ("files_read", v.get("files_read").cloned().unwrap_or(Json::Null)),
        // The functions the run called, per unit (the recorder's `coverage`,
        // from instrumented units: key -> slot ranges; null without): what
        // `"cold"` keeps in each unit's primary module.
        ("coverage", v.get("coverage").cloned().unwrap_or(Json::Null)),
        ("page_source", s(web_src.display().to_string())),
        ("session", s(sha256(text.as_bytes()))),
    ]);
    write_file(&out.join(crate::project::PROJECT_FILE), json::write(&project).as_bytes(), false)?;
    write_file(&out.join("build.sh"), BUILD_SH.as_bytes(), true)?;
    write_file(&out.join(".gitignore"), b"/build/\n", false)?;
    if !out.join(crate::config::CONFIG_FILE).exists() {
        write_file(&out.join(crate::config::CONFIG_FILE), template(&o.features_off).as_bytes(), false)?;
    }
    // The page: the user's, an example to start from (app.mjs's start()).
    crate::web::write_shell(out)?;
    Ok(sum)
}

const BUILD_SH: &str = "#!/bin/sh
# Generated by `gexc eject`: build this W3 page's site inputs into build/web
# (the page with user/eject.json's features, the bundle, the units).
# Then: node <gex>/web/eject-site.mjs . SITE
set -e
cd \"$(dirname \"$0\")\"
exec runtime/gexc eject build .
";

/// A W3 project's `user/eject.json`: the features, the ones given off.
fn template(off: &[String]) -> String {
    json::write(&obj(vec![
        (
            "about",
            s("Your settings; gexc eject never rewrites this file. \"features\": the parts of gex's \
               runtime the build keeps, false leaves one out (docs/eject.md, \"Features\"); \
               rebuild with ./build.sh. \"bundle\": \"split\" ships the files the recorded run \
               opened in the site's bundle and the rest as lazy files, fetched when the program \
               first reads one (\"whole\": everything in the bundle); see docs/eject.md, \
               \"The bundle split\". \"cold\": \"none\" ships every unit whole in one archive; a clustering \
               (\"chunk:256\", ...) splits off each unit's code the recording never ran and \
               fetches it on first call, \"instrument\" builds units that record the functions a \
               run calls; see docs/eject.md, \"Cold code\". \
               \"start\": defaults for the app library's start() \
               (args, env, cwd, size, mtimes, strace, trapstack, persist, ready), over the \
               recording's; the page's own options win (docs/eject.md, \"The web build\")."),
        ),
        (
            "features",
            obj(crate::features::FEATURES.iter().map(|f| (f.name, json::b(!off.iter().any(|x| x == f.name)))).collect()),
        ),
        ("bundle", s("split")),
        ("cold", s(COLD_DEFAULT)),
        ("start", obj(vec![])),
    ]))
}

#[derive(Debug, Default)]
pub struct W3Report {
    pub features_off: Vec<String>,
    pub page_files: usize,
    pub units: usize,
}

/// Slot ranges, `a-b,c,...` (the recorder's coverage of one unit).
fn slot_ranges(r: &str) -> Result<std::collections::HashSet<u32>> {
    let mut hot = std::collections::HashSet::new();
    for x in r.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        match x.split_once('-') {
            Some((a, b)) => hot.extend(a.parse::<u32>()?..=b.parse::<u32>()?),
            None => {
                hot.insert(x.parse::<u32>()?);
            }
        }
    }
    Ok(hot)
}

/// A new W3 project's `"cold"`: none. Every unit ships whole, in the site's
/// one units archive (web/eject-site.mjs), and nothing is fetched on a call.
/// A split (`"chunk:256"` and the other clusterings) is opt-in: it cuts the
/// download and costs a fetch per cluster at first call (docs/eject.md,
/// "Cold code").
const COLD_DEFAULT: &str = "none";

/// `gexc eject build PROJECT` for a W3 project.
pub fn build(dir: &Path) -> Result<W3Report> {
    let p = dir.join(crate::project::PROJECT_FILE);
    let v = Json::parse(&std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?)
        .map_err(|e| anyhow!("{}: {e}", p.display()))?;
    let what = crate::project::PROJECT_FILE;
    let (config, config_bytes) = Config::load(dir)?;
    crate::features::check(&config.features_off)?;
    let off = config.features_off.clone();
    let has = |f: &str| !off.iter().any(|x| x == f);

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
    for f in RUNTIME_WASM {
        let to = page.join("gex").join(f);
        let _ = std::fs::remove_file(&to);
        link_or_copy(&dir.join("runtime/web/gex").join(f), &to)?;
    }
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
    link_or_copy(&dir.join("bundle/rootfs.tar"), &web.join("rootfs.tar"))?;
    let units = list(&v, "units");
    let instrument = config.cold.as_deref() == Some("instrument");
    let coverage = match (&config.cold, v.get("coverage")) {
        (None, _) => None,
        _ if instrument => None,
        (Some(_), Some(Json::Obj(kv))) => Some(kv),
        (Some(_), _) => {
            eprintln!(
                "gexc eject build: user/eject.json says \"cold\", but the recording has no coverage: \
                 the units stay whole (docs/eject.md, \"Cold code\": record a site built with \
                 \"cold\": \"instrument\" with record-w3.mjs --coverage-into)"
            );
            None
        }
    };
    let mut cold_units = 0;
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
        let to = web.join("units").join(&key);
        link_or_copy(&dir.join("units").join(&key).join("c.entries"), &to.join("c.entries"))?;
        if instrument {
            let wasm = std::fs::read(dir.join("units").join(&key).join("c.wasm"))?;
            write_file(&to.join("c.wasm"), &gex_split::instrument(&wasm)?, false)?;
            continue;
        }
        if let Some(w) = pruned.get(&key) {
            write_file(&to.join("c.wasm"), w, false)?;
            continue;
        }
        let (Some(cov), Some(how)) = (coverage, config.cold.as_deref()) else {
            link_or_copy(&dir.join("units").join(&key).join("c.wasm"), &to.join("c.wasm"))?;
            continue;
        };
        let hot = match cov.iter().find(|(k, _)| *k == key).map(|(_, r)| r) {
            Some(Json::Str(r)) => slot_ranges(r).with_context(|| format!("the recording's coverage of {key}"))?,
            _ => std::collections::HashSet::new(),
        };
        let wasm = std::fs::read(dir.join("units").join(&key).join("c.wasm"))?;
        let (primary, clusters, _) = gex_split::split(&wasm, &hot, how.parse()?)
            .with_context(|| format!("splitting unit {key}"))?;
        write_file(&to.join("c.wasm"), &primary, false)?;
        std::fs::create_dir_all(to.join("cold"))?;
        for (i, c) in clusters.iter().enumerate() {
            write_file(&to.join("cold").join(format!("{i}.wasm")), c, false)?;
        }
        cold_units += 1;
    }

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
    if cold_units > 0 {
        params.push(("cold".to_string(), s("1")));
    }
    // `"bundle": "split"`: the files the recorded run opened, which the site
    // keeps in its bundle (web/eject-site.mjs).
    let bundle = if config.bundle_split {
        match v.get("files_read") {
            Some(Json::Arr(read)) => obj(vec![("split", json::b(true)), ("read", Json::Arr(read.clone()))]),
            _ => bail!(
                "user/eject.json says \"bundle\": \"split\", but the recording has no files_read \
                 (record with a tools/eject/record-w3.mjs and page that have the read set, or set \"bundle\": \"whole\")"
            ),
        }
    } else {
        obj(vec![("split", json::b(false))])
    };
    let launch = obj(vec![
        ("kind", s(KIND)),
        ("exe", rec.get("exe").cloned().unwrap_or(Json::Null)),
        ("argv", rec.get("argv").cloned().unwrap_or(Json::Arr(Vec::new()))),
        ("env", rec.get("env").cloned().unwrap_or(Json::Arr(Vec::new()))),
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
        ("params", Json::Obj(params)),
        ("features_off", strs(&off)),
        ("units", Json::Arr(units.to_vec())),
        ("bundle", bundle),
        // user/eject.json "start": the app library's start() defaults, over
        // the recording's (docs/eject.md, "The web build").
        ("start", Json::parse(&config.start).map_err(|e| anyhow!("user/eject.json \"start\": {e}"))?),
    ]);
    write_file(&web.join("w3.json"), json::write(&launch).as_bytes(), false)?;
    write_file(
        &dir.join("build/build.json"),
        json::write(&obj(vec![
            ("kind", s(KIND)),
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
    Ok(W3Report { features_off: off, page_files, units: units.len() })
}
