//! Which functions a build ships as unit code, given a [`Profile`].
//!
//! Every unit ships: the slots chosen as compiled code, every other slot a
//! stub for the fallback (the function JIT, or a cold cluster). A slot is
//! worth shipping when what it saves at start-up beats what it costs to
//! download and compile:
//!
//! - its cost, `bytes / link + compile`: its body's bytes in the unit,
//!   compressed at its unit's ratio, over the link; and the page's compile
//!   time for that many bytes;
//! - its saving, the fallback's cost for it, `fixed + per_kib * KiB +
//!   per_call * calls`: translating it (about its size) and running it the
//!   slower way, counted in full if the run first entered it before the
//!   start-up mark (`desktop`), at `after_weight` if after it, and not at all
//!   if never.
//!
//! The slots are taken in order of saving over cost, and the sweep reports,
//! for each prefix, the bytes shipped and the predicted download plus
//! fallback time; the chosen point is the least. The model only picks
//! candidates: the measured runs decide.
use crate::profile::Profile;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Params {
    /// The link, bytes per second.
    pub link_bps: f64,
    /// The page's compile time, ms per MiB of unit code.
    pub compile_ms_per_mib: f64,
    /// The fallback's cost per function: fixed, per KiB of the function's
    /// unit body, and per call.
    pub fb_fixed_ms: f64,
    pub fb_ms_per_kib: f64,
    pub fb_ms_per_call: f64,
    /// The start-up mark.
    pub mark: String,
    /// The weight of a function first entered after the mark.
    pub after_weight: f64,
    /// zstd level for each unit's compression ratio.
    pub zstd_level: i32,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            link_bps: 15e6 / 8.0,
            compile_ms_per_mib: 60.0,
            fb_fixed_ms: 0.3,
            fb_ms_per_kib: 0.5,
            fb_ms_per_call: 0.0005,
            mark: "desktop".to_string(),
            after_weight: 0.1,
            zstd_level: 9,
        }
    }
}

/// One unit's sizes.
pub struct UnitCost {
    pub key: String,
    /// Compressed bytes of the unit whole.
    pub whole: u64,
    /// Compressed over raw.
    pub ratio: f64,
    /// Compressed bytes with every slot a stub.
    pub stubs: u64,
    /// Each slot's body, raw bytes.
    pub body: Vec<u32>,
}

fn zsize(bytes: &[u8], level: i32) -> Result<u64> {
    use std::io::Write;
    let mut c = std::process::Command::new("zstd")
        .args([format!("-{level}"), "-q".into(), "-c".into(), "--long=27".into()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .context("running zstd")?;
    let mut sin = c.stdin.take().unwrap();
    let data = bytes.to_vec();
    let w = std::thread::spawn(move || sin.write_all(&data));
    let out = c.wait_with_output()?;
    w.join().unwrap()?;
    anyhow::ensure!(out.status.success(), "zstd failed");
    Ok(out.stdout.len() as u64)
}

/// The sizes of the unit in `dir` (a unit cache's `<key>`).
pub fn unit_cost(dir: &Path, key: &str, level: i32) -> Result<UnitCost> {
    let wasm = std::fs::read(dir.join("c.wasm")).with_context(|| format!("reading {}/c.wasm", dir.display()))?;
    let u = gex_split::Unit::parse(&wasm).with_context(|| format!("unit {key}"))?;
    let body: Vec<u32> = (0..u.slots.len() as u32).map(|k| u.body_size(k) as u32).collect();
    let whole = zsize(&wasm, level)?;
    let (stub, _) = gex_split::jit(&wasm, &HashSet::new()).with_context(|| format!("unit {key}"))?;
    let stubs = zsize(&stub, level)?;
    Ok(UnitCost { key: key.to_string(), whole, ratio: whole as f64 / wasm.len().max(1) as f64, stubs, body })
}

/// A candidate slot.
#[derive(Clone, Debug)]
pub struct Cand {
    pub unit: usize,
    pub slot: u32,
    /// Compressed bytes shipped for it.
    pub bytes: f64,
    /// Its download and compile, ms.
    pub cost_ms: f64,
    /// Its fallback cost, ms, weighted.
    pub save_ms: f64,
    pub before_mark: bool,
}

/// One point of the sweep: the first `k` candidates shipped.
#[derive(Clone, Debug)]
pub struct Point {
    pub k: usize,
    /// Compressed bytes of every unit (stubs plus the shipped bodies).
    pub bytes: f64,
    pub download_ms: f64,
    pub compile_ms: f64,
    /// The fallback's time for what is not shipped, before the mark.
    pub fallback_ms: f64,
    /// ... and after it (unweighted).
    pub fallback_after_ms: f64,
    /// download + compile + fallback before the mark + weighted after.
    pub total_ms: f64,
}

pub struct Sweep {
    pub cands: Vec<Cand>,
    pub points: Vec<Point>,
    pub best: usize,
    /// The whole units' bytes, for the table.
    pub whole_bytes: f64,
}

/// The candidates in order and the sweep over them.
pub fn sweep(units: &[UnitCost], prof: &Profile, p: &Params) -> Sweep {
    let mark = prof.marks.get(&p.mark).copied().unwrap_or(f64::INFINITY);
    let mut cands = Vec::new();
    let mut stub_bytes = 0.0;
    let mut whole_bytes = 0.0;
    for (i, u) in units.iter().enumerate() {
        stub_bytes += u.stubs as f64;
        whole_bytes += u.whole as f64;
        let Some(m) = prof.units.get(&u.key) else { continue };
        for (&s, use_) in m {
            let Some(&raw) = u.body.get(s as usize) else { continue };
            if !use_.first_ms.is_finite() && use_.calls == 0 {
                continue;
            }
            let bytes = raw as f64 * u.ratio;
            let cost_ms = bytes / p.link_bps * 1000.0 + raw as f64 / 1048576.0 * p.compile_ms_per_mib;
            let fb = p.fb_fixed_ms + p.fb_ms_per_kib * raw as f64 / 1024.0 + p.fb_ms_per_call * use_.calls as f64;
            let before = use_.first_ms <= mark;
            let save_ms = if before { fb } else { fb * p.after_weight };
            cands.push(Cand { unit: i, slot: s, bytes, cost_ms, save_ms, before_mark: before });
        }
    }
    cands.sort_by(|a, b| {
        let da = a.save_ms / a.cost_ms.max(1e-9);
        let db = b.save_ms / b.cost_ms.max(1e-9);
        db.partial_cmp(&da).unwrap_or(std::cmp::Ordering::Equal)
    });
    // Every prefix, the totals (the curve is evaluated at each k; the
    // reported points are a sample of it plus the least).
    let unw = |c: &Cand| if c.before_mark { c.save_ms } else { c.save_ms / p.after_weight.max(1e-9) };
    let mut fb_before: f64 = cands.iter().filter(|c| c.before_mark).map(|c| c.save_ms).sum();
    let mut fb_after: f64 = cands.iter().filter(|c| !c.before_mark).map(unw).sum();
    let mut bytes = stub_bytes;
    let mut compile = 0.0;
    let mut all = Vec::with_capacity(cands.len() + 1);
    let point = |k, bytes: f64, compile: f64, fb_before: f64, fb_after: f64| {
        let download_ms = bytes / p.link_bps * 1000.0;
        Point {
            k,
            bytes,
            download_ms,
            compile_ms: compile,
            fallback_ms: fb_before,
            fallback_after_ms: fb_after,
            total_ms: download_ms + compile + fb_before + p.after_weight * fb_after,
        }
    };
    all.push(point(0, bytes, compile, fb_before, fb_after));
    for (i, c) in cands.iter().enumerate() {
        bytes += c.bytes;
        let raw = units[c.unit].body[c.slot as usize] as f64;
        compile += raw / 1048576.0 * p.compile_ms_per_mib;
        if c.before_mark {
            fb_before -= c.save_ms;
        } else {
            fb_after -= unw(c);
        }
        all.push(point(i + 1, bytes, compile, fb_before.max(0.0), fb_after.max(0.0)));
    }
    let best = all
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.total_ms.partial_cmp(&b.1.total_ms).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(0, |(i, _)| i);
    Sweep { cands, points: all, best, whole_bytes }
}

/// The slots of the first `k` candidates, per unit key, as ranges
/// (`select.json`: `{"KEY": "a-b,c", ...}`; a unit not named ships stubs only).
pub fn selection(units: &[UnitCost], s: &Sweep, k: usize) -> BTreeMap<String, String> {
    let mut by: BTreeMap<String, HashSet<u32>> = BTreeMap::new();
    for c in &s.cands[..k.min(s.cands.len())] {
        by.entry(units[c.unit].key.clone()).or_default().insert(c.slot);
    }
    by.into_iter().map(|(k, v)| (k, crate::ranges_of(&v))).collect()
}

/// `select.json`'s text.
pub fn write_selection(sel: &BTreeMap<String, String>) -> String {
    let mut o = String::from("{\n");
    let n = sel.len();
    for (i, (k, v)) in sel.iter().enumerate() {
        o.push_str(&format!("  \"{k}\": \"{v}\"{}\n", if i + 1 < n { "," } else { "" }));
    }
    o.push_str("}\n");
    o
}

/// Read `select.json` (a flat object of strings, as [`write_selection`]
/// writes it).
pub fn read_selection(text: &str) -> Result<BTreeMap<String, String>> {
    // Every string in order, key then value (a value's ranges have commas,
    // no string has a quote).
    let strs: Vec<&str> = text.split('"').skip(1).step_by(2).collect();
    anyhow::ensure!(strs.len() % 2 == 0, "select.json: a key without a value");
    Ok(strs.chunks(2).map(|kv| (kv[0].to_string(), kv[1].to_string())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Profile, Use};

    #[test]
    fn sweep_prefers_hot_small_functions() {
        let units = vec![UnitCost { key: "k".into(), whole: 1000, ratio: 0.5, stubs: 100, body: vec![100, 100_000, 100, 100] }];
        let mut prof = Profile::default();
        prof.marks.insert("desktop".into(), 1000.0);
        let m = prof.units.entry("k".into()).or_default();
        m.insert(0, Use { first_ms: 10.0, calls: 1_000_000 });
        m.insert(1, Use { first_ms: 10.0, calls: 1 });
        m.insert(2, Use { first_ms: 5000.0, calls: 1 });
        let p = Params { link_bps: 1e5, ..Params::default() };
        let s = sweep(&units, &prof, &p);
        assert_eq!(s.cands[0].slot, 0);
        assert_eq!(s.cands.len(), 3);
        assert!(s.best >= 1);
        let sel = selection(&units, &s, 1);
        assert_eq!(sel["k"], "0");
        let two = read_selection("{\n  \"a\": \"1-3,7\",\n  \"b\": \"0\"\n}\n").unwrap();
        assert_eq!((two["a"].as_str(), two["b"].as_str()), ("1-3,7", "0"));
        assert_eq!(read_selection(&write_selection(&sel)).unwrap(), sel);
    }
}
