//! What a build ships of its function-tier units.
//!
//! A unit (`<cache>/<key>/c.wasm` and `c.entries`, gex-cgen's layout) holds
//! every guest function of its image. A build ships each unit in one of the
//! [`Shape`]s: whole, instrumented (every function reports its entry, for a
//! coverage or profile run), pruned to a proof's set, or cut to a set of
//! slots with the rest split into cold clusters the page fetches on first
//! call, or left to the page's function JIT.
//!
//! The output has the unit cache's layout, so both the page (`?cgen=` with
//! `?cgenurl=`, which reads `<key>/c.wasm` and `c.entries`, and a split
//! unit's `cold/`) and an eject (gex-eject, which packs it) read it, for PE
//! and ELF images alike: nothing here depends on the image's format.
pub mod profile;
pub mod select;

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

pub use gex_split::Clustering;

/// How a unit is shipped.
pub enum Shape<'a> {
    /// As built.
    Whole,
    /// Every guest function calls `gexcov.hit(slot)` on entry.
    Instrument,
    /// The module a prune made (gex-eject's proof), as given.
    Pruned(&'a [u8]),
    /// The slots in the set compiled; every other slot a stub for the
    /// function JIT, its address unpublished (`gex_split::jit`).
    Jit(&'a HashSet<u32>),
    /// The slots in the set in the primary; the rest in cold clusters
    /// (`gex_split::split`).
    Split(&'a HashSet<u32>, Clustering),
}

/// What [`ship`] made of one unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shipped {
    /// It leaves functions to the function JIT.
    pub jit: bool,
    /// It has cold clusters.
    pub cold: bool,
}

/// Hard-link `from` to `to`, or copy it where a link cannot be made.
pub fn link_or_copy(from: &Path, to: &Path) -> Result<u64> {
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    if std::fs::hard_link(from, to).is_err() {
        std::fs::copy(from, to).with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    }
    Ok(std::fs::metadata(to)?.len())
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting the mode of {}", path.display()))?;
    Ok(())
}

/// Ship the unit in `from` (a unit cache's `<key>` directory) into `to` in
/// `shape`: `c.wasm`, `c.entries`, and a split's `cold/N.wasm`.
pub fn ship(from: &Path, to: &Path, shape: Shape) -> Result<Shipped> {
    let entries = from.join("c.entries");
    let wasm_path = from.join("c.wasm");
    if !matches!(shape, Shape::Jit(_)) {
        link_or_copy(&entries, &to.join("c.entries"))?;
    }
    match shape {
        Shape::Whole => {
            link_or_copy(&wasm_path, &to.join("c.wasm"))?;
            Ok(Shipped::default())
        }
        Shape::Instrument => {
            let wasm = read(&wasm_path)?;
            write(&to.join("c.wasm"), &gex_split::instrument(&wasm)?)?;
            Ok(Shipped::default())
        }
        Shape::Pruned(w) => {
            write(&to.join("c.wasm"), w)?;
            Ok(Shipped::default())
        }
        Shape::Jit(hot) => {
            // Only the set is the unit's; the rest is the function JIT's,
            // translated in the page on first call.
            let wasm = read(&wasm_path)?;
            let (module, entries) = gex_split::jit(&wasm, hot)?;
            write(&to.join("c.wasm"), &module)?;
            write(&to.join("c.entries"), &entries.iter().flat_map(|a| a.to_le_bytes()).collect::<Vec<u8>>())?;
            Ok(Shipped { jit: true, cold: false })
        }
        Shape::Split(hot, how) => {
            let wasm = read(&wasm_path)?;
            let (primary, clusters, _) = gex_split::split(&wasm, hot, how)?;
            write(&to.join("c.wasm"), &primary)?;
            std::fs::create_dir_all(to.join("cold"))?;
            for (i, c) in clusters.iter().enumerate() {
                write(&to.join("cold").join(format!("{i}.wasm")), c)?;
            }
            Ok(Shipped { jit: false, cold: true })
        }
    }
}

fn read(p: &Path) -> Result<Vec<u8>> {
    std::fs::read(p).with_context(|| format!("reading {}", p.display()))
}

/// Slot ranges, `a-b,c,...` (a coverage's or a selection's set of one unit).
pub fn slot_ranges(r: &str) -> Result<HashSet<u32>> {
    let mut hot = HashSet::new();
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

/// A set of slots as ranges, `a-b,c,...`: [`slot_ranges`]'s inverse.
pub fn ranges_of(set: &HashSet<u32>) -> String {
    let mut xs: Vec<u32> = set.iter().copied().collect();
    xs.sort_unstable();
    let mut o = Vec::new();
    let mut i = 0;
    while i < xs.len() {
        let mut j = i;
        while j + 1 < xs.len() && xs[j + 1] == xs[j] + 1 {
            j += 1;
        }
        o.push(if i == j { xs[i].to_string() } else { format!("{}-{}", xs[i], xs[j]) });
        i = j + 1;
    }
    o.join(",")
}
