//! `gexc units`: what a build ships of its function-tier units (gex-units),
//! for the page's unit cache and an eject alike.
//!
//! - `gexc units profile OUT LOG...`: the `[gex-prof]` and `[gex-mark]` lines
//!   of instrumented runs' consoles (and earlier profiles, `*.prof`), added
//!   up into OUT.
//! - `gexc units select PROFILE CACHE OUT [options]`: the sweep (gex-units
//!   `select`) over the units in CACHE the profile names, printed; OUT
//!   (`select.json`) gets the least point's slots, or `--point K`'s, or the
//!   largest point within `--budget BYTES`. Options: `--link-mbit N` (15),
//!   `--compile-ms-per-mib N`, `--fb-fixed-ms N`, `--fb-ms-per-kib N`,
//!   `--fb-ms-per-call N`, `--mark NAME` (desktop), `--after-weight N`
//!   (0.1), `--zstd N` (9), `--points N` (rows printed, 24), `--units LIST`
//!   (a file of unit keys, one a line: only those; default every unit in
//!   CACHE the profile or the cache holds).
//! - `gexc units build CACHE SELECT OUT [--fallback jit|CLUSTERING]`: each
//!   unit of CACHE into OUT, a unit cache: the slots SELECT names compiled,
//!   every other slot left to the fallback (the function JIT, or cold
//!   clusters); `--whole` ships every unit whole, `--keys LIST` only those
//!   (and `--providers LIST` only those providers), `--whole-keys LIST`
//!   those whole whatever SELECT says;
//!   `--preload PREFIX` writes OUT/preload.json for the page (PREFIX: the
//!   URL OUT is served at, the page's `cgenurl`): the units SELECT names and
//!   the whole ones.
use anyhow::{bail, Context, Result};
use gex_units::profile::Profile;
use gex_units::select;
use std::path::{Path, PathBuf};

fn opt(args: &[String], n: &str) -> Option<String> {
    args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned()
}

fn has(args: &[String], n: &str) -> bool {
    args.iter().any(|a| a == n)
}

fn positional(args: &[String]) -> Vec<String> {
    let flags = ["--whole"];
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            i += if flags.contains(&a.as_str()) { 1 } else { 2 };
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

fn num(args: &[String], n: &str, d: f64) -> Result<f64> {
    opt(args, n).map_or(Ok(d), |v| v.parse().with_context(|| format!("{n} {v}")))
}

/// The unit keys of a unit cache: its directories with a `c.wasm`.
fn cache_keys(cache: &Path) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for e in std::fs::read_dir(cache).with_context(|| format!("reading {}", cache.display()))? {
        let e = e?;
        let n = e.file_name().to_string_lossy().to_string();
        if n.len() == 64 && n.bytes().all(|b| b.is_ascii_hexdigit()) && e.path().join("c.wasm").is_file() {
            keys.push(n);
        }
    }
    keys.sort();
    Ok(keys)
}

fn key_list(args: &[String], flag: &str, cache: &Path) -> Result<Vec<String>> {
    match opt(args, flag) {
        Some(f) => Ok(std::fs::read_to_string(&f)
            .with_context(|| format!("reading {f}"))?
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()),
        None => cache_keys(cache),
    }
}

pub fn cmd_units(args: &[String]) -> Result<()> {
    let usage = "usage: gexc units profile OUT LOG... | select PROFILE CACHE OUT [options] | build CACHE SELECT OUT [--fallback jit|CLUSTERING] [--whole] [--keys LIST] [--whole-keys LIST] [--providers LIST] [--preload PREFIX]";
    let Some(what) = args.first() else { bail!("{usage}") };
    let rest = &args[1..];
    let pos = positional(rest);
    match what.as_str() {
        "profile" => {
            let Some((out, logs)) = pos.split_first() else { bail!("{usage}") };
            if logs.is_empty() {
                bail!("{usage}");
            }
            let mut p = Profile::default();
            for l in logs {
                let text = String::from_utf8_lossy(&std::fs::read(l).with_context(|| format!("reading {l}"))?).to_string();
                if l.ends_with(".prof") {
                    p.parse_into(&text).with_context(|| l.clone())?;
                    continue;
                }
                let n = p.add_log(&text).with_context(|| l.clone())?;
                if n == 0 {
                    eprintln!("gexc units profile: {l}: no [gex-prof] lines (a site built with \"cold\": \"instrument\"?)");
                }
            }
            std::fs::write(out, p.write()).with_context(|| format!("writing {out}"))?;
            let slots: usize = p.units.values().map(|m| m.len()).sum();
            let marks: Vec<String> = p.marks.iter().map(|(n, t)| format!("{n} {:.1} s", t / 1000.0)).collect();
            eprintln!("gexc units profile: {out}: {} units, {slots} slots; marks: {}", p.units.len(), marks.join(", "));
        }
        "select" => {
            let (Some(pf), Some(cache), Some(out)) = (pos.first(), pos.get(1), pos.get(2)) else { bail!("{usage}") };
            let mut prof = Profile::default();
            prof.parse_into(&std::fs::read_to_string(pf).with_context(|| format!("reading {pf}"))?)?;
            let d = select::Params::default();
            let p = select::Params {
                link_bps: num(rest, "--link-mbit", d.link_bps * 8.0 / 1e6)? * 1e6 / 8.0,
                compile_ms_per_mib: num(rest, "--compile-ms-per-mib", d.compile_ms_per_mib)?,
                fb_fixed_ms: num(rest, "--fb-fixed-ms", d.fb_fixed_ms)?,
                fb_ms_per_kib: num(rest, "--fb-ms-per-kib", d.fb_ms_per_kib)?,
                fb_ms_per_call: num(rest, "--fb-ms-per-call", d.fb_ms_per_call)?,
                mark: opt(rest, "--mark").unwrap_or(d.mark),
                after_weight: num(rest, "--after-weight", d.after_weight)?,
                zstd_level: num(rest, "--zstd", d.zstd_level as f64)? as i32,
            };
            let cache = PathBuf::from(cache);
            let keys = key_list(rest, "--units", &cache)?;
            // Each unit's sizes, in parallel (each a zstd of its module and of its stubs).
            let next = std::sync::atomic::AtomicUsize::new(0);
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(12);
            let slots: Vec<std::sync::Mutex<Option<Result<select::UnitCost>>>> = keys.iter().map(|_| std::sync::Mutex::new(None)).collect();
            std::thread::scope(|sc| {
                for _ in 0..threads {
                    sc.spawn(|| loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(k) = keys.get(i) else { break };
                        *slots[i].lock().unwrap() = Some(select::unit_cost(&cache.join(k), k, p.zstd_level));
                    });
                }
            });
            let units: Vec<select::UnitCost> =
                slots.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect::<Result<_>>()?;
            let s = select::sweep(&units, &prof, &p);
            let k = if let Some(k) = opt(rest, "--point") {
                k.parse::<usize>()?
            } else if let Some(b) = opt(rest, "--budget") {
                let b: f64 = b.parse()?;
                s.points.iter().filter(|x| x.bytes <= b).map(|x| x.k).max().unwrap_or(0)
            } else {
                s.points[s.best].k
            };
            let rows = num(rest, "--points", 24.0)? as usize;
            print_sweep(&s, &p, rows, k);
            let sel = select::selection(&units, &s, k);
            std::fs::write(out, select::write_selection(&sel)).with_context(|| format!("writing {out}"))?;
            eprintln!("gexc units select: {out}: point {k} ({} slots in {} units)", k.min(s.cands.len()), sel.len());
        }
        "build" => {
            let (Some(cache), Some(selp), Some(out)) = (pos.first(), pos.get(1), pos.get(2)) else { bail!("{usage}") };
            let cache = PathBuf::from(cache);
            let out = PathBuf::from(out);
            let whole = has(rest, "--whole");
            let sel = if whole {
                Default::default()
            } else {
                select::read_selection(&std::fs::read_to_string(selp).with_context(|| format!("reading {selp}"))?)?
            };
            let fallback = opt(rest, "--fallback").unwrap_or_else(|| "jit".to_string());
            let how: Option<gex_units::Clustering> = if fallback == "jit" { None } else { Some(fallback.parse()?) };
            let mut keys = key_list(rest, "--keys", &cache)?;
            // `--whole-keys LIST`: units shipped whole whatever SELECT says
            // (one no image has, as the NT trampolines').
            let whole_keys: Vec<String> = match opt(rest, "--whole-keys") {
                Some(_) => key_list(rest, "--whole-keys", &cache)?,
                None => Vec::new(),
            };
            for k in &whole_keys {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
            let (mut jit, mut cold, mut kept) = (0, 0, 0);
            for k in &keys {
                let hot = match sel.get(k) {
                    Some(r) => gex_units::slot_ranges(r)?,
                    None => Default::default(),
                };
                let shape = if whole || whole_keys.contains(k) {
                    gex_units::Shape::Whole
                } else if let Some(how) = how {
                    gex_units::Shape::Split(&hot, how)
                } else {
                    gex_units::Shape::Jit(&hot)
                };
                let s = gex_units::ship(&cache.join(k), &out.join(k), shape).with_context(|| format!("unit {k}"))?;
                jit += s.jit as usize;
                cold += s.cold as usize;
                kept += hot.len();
            }
            // The providers the units bind, as in a unit cache: every one, or
            // those `--providers LIST` names (a file of provider file names).
            let only: Option<Vec<String>> = match opt(rest, "--providers") {
                Some(f) => Some(std::fs::read_to_string(&f).with_context(|| format!("reading {f}"))?.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()),
                None => None,
            };
            if let Ok(rd) = std::fs::read_dir(cache.join("providers")) {
                for e in rd.flatten() {
                    if only.as_ref().is_some_and(|o| !o.iter().any(|n| e.file_name().to_string_lossy() == n.as_str())) {
                        continue;
                    }
                    gex_units::link_or_copy(&e.path(), &out.join("providers").join(e.file_name()))?;
                }
            }
            // `--preload PREFIX`: OUT/preload.json, the page's list of what to
            // fetch and compile while it loads (app.js `preloadUnits`), every
            // unit and provider under PREFIX, the URL the page serves OUT at
            // (its `cgenurl`): no unit is a synchronous fetch at first map.
            // With a selection, only its units and the whole ones: a unit of
            // stubs alone is fetched when a process first maps it.
            if let Some(prefix) = opt(rest, "--preload") {
                let prefix = prefix.trim_end_matches('/');
                let mut items: Vec<(u64, String)> = Vec::new();
                for k in keys.iter().filter(|k| whole || whole_keys.contains(k) || sel.contains_key(k.as_str())) {
                    let w = std::fs::metadata(out.join(k).join("c.wasm")).map_or(0, |m| m.len());
                    items.push((w, format!("[\"cgen:{k}\",\"{prefix}/{k}/c.wasm\"]")));
                    items.push((0, format!("[\"file:{k}/c.entries\",\"{prefix}/{k}/c.entries\"]")));
                    if out.join(k).join("cold").is_dir() {
                        bail!("--preload with a split unit ({k}): its cold clusters are fetched on first call");
                    }
                }
                if let Ok(rd) = std::fs::read_dir(out.join("providers")) {
                    let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n.ends_with(".wasm")).collect();
                    names.sort();
                    for n in names {
                        items.push((0, format!("[\"file:providers/{n}\",\"{prefix}/providers/{n}\"]")));
                    }
                }
                // The largest modules first: their compiles start earliest.
                items.sort_by(|a, b| b.0.cmp(&a.0));
                let text = format!("[{}]\n", items.into_iter().map(|(_, x)| x).collect::<Vec<_>>().join(","));
                std::fs::write(out.join("preload.json"), text)?;
            }
            eprintln!(
                "gexc units build: {}: {} units ({jit} on the function JIT, {cold} split), {kept} slots compiled",
                out.display(),
                keys.len()
            );
        }
        _ => bail!("{usage}"),
    }
    Ok(())
}

fn print_sweep(s: &select::Sweep, p: &select::Params, rows: usize, chosen: usize) {
    let n = s.points.len();
    let mut ks: Vec<usize> = (0..rows.max(2)).map(|i| ((n - 1) as f64 * (i as f64 / (rows.max(2) - 1) as f64).powi(3)) as usize).collect();
    ks.push(s.best);
    ks.push(chosen.min(n - 1));
    ks.sort_unstable();
    ks.dedup();
    println!(
        "link {:.1} Mbit/s; whole units {:.1} MB; {} candidate slots; mark {}",
        p.link_bps * 8.0 / 1e6,
        s.whole_bytes / 1e6,
        s.cands.len(),
        p.mark
    );
    println!("{:>8} {:>9} {:>11} {:>10} {:>12} {:>13} {:>10}", "k", "MB", "download s", "compile s", "fallback s", "after mark s", "total s");
    for k in ks {
        let x = &s.points[k];
        let tag = if k == s.best { " least" } else if k == chosen { " chosen" } else { "" };
        println!(
            "{:>8} {:>9.2} {:>11.2} {:>10.2} {:>12.2} {:>13.2} {:>10.2}{tag}",
            x.k,
            x.bytes / 1e6,
            x.download_ms / 1e3,
            x.compile_ms / 1e3,
            x.fallback_ms / 1e3,
            x.fallback_after_ms / 1e3,
            x.total_ms / 1e3
        );
    }
}
