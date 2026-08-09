//! Which objects a program actually consists of.
//!
//! `gexc build` translated the image it was given and nothing else, and on a
//! normally linked program that is the wrapper rather than the work:
//! `apt-get` is 67 KB of argument parsing in front of 1.9 MB of
//! `libapt-pkg.so`. Statically recompiling it produced 212 blocks. perl only
//! ever looked like a whole-program build because Debian links libperl into
//! the executable, which is a fact about Debian's packaging and not about
//! this compiler.
//!
//! So: read `DT_NEEDED` transitively and say what the program is. This is
//! the same walk `ld.so` does, and it is done here without running anything,
//! which is what "ahead of time" has to mean.
//!
//! The search order is `ld.so`'s, minus the parts that cannot matter here:
//! there is no `/etc/ld.so.cache` in a sysroot that was unpacked rather than
//! installed -- the trace of a real run shows the guest's loader looking for
//! one and being told `-2` -- and the hardware-capability subdirectories
//! (`tls/`, `atomics/`, `aarch64/`) never contain anything in a Debian
//! rootfs, so a miss there costs a `stat` and finds nothing. What is left is
//! `DT_RUNPATH`, then the multiarch directories, then `/lib` and `/usr/lib`.

use crate::elf::{load, Image, LoadError};
use std::collections::{BTreeMap, HashSet};

/// One object in a program's closure, already loaded at its assigned base.
#[derive(Clone, Debug)]
pub struct Object {
    /// Where it was found in the guest's namespace.
    pub path: String,
    /// `DT_SONAME`, or the file's basename when it has none.
    pub name: String,
    /// The image, placed at `image.bias`.
    pub image: Image,
    /// Bytes of guest address space it occupies, from its base.
    pub span: u64,
    /// How many objects deep it was reached: 0 for the executable, 1 for what
    /// the executable names, and so on. Only a diagnostic.
    pub depth: u32,
}

/// A program and everything it links against.
#[derive(Clone, Debug, Default)]
pub struct Closure {
    /// The executable first, then its dependencies in breadth-first order --
    /// which is the order `ld.so` searches for a symbol, and so the order in
    /// which two definitions of one name resolve.
    pub objects: Vec<Object>,
    /// Names that could not be found, with the object that asked for them.
    pub missing: Vec<(String, String)>,
}

impl Closure {
    /// Total executable bytes across the closure -- what a whole-program
    /// build would have to translate.
    pub fn text_bytes(&self) -> u64 {
        self.objects
            .iter()
            .map(|o| {
                let (lo, hi) = o.image.text_range;
                hi.saturating_sub(lo)
            })
            .sum()
    }
}

/// The directories `ld.so` looks in, after any `DT_RUNPATH`.
///
/// aarch64 only, because that is the only guest architecture there is.
const DEFAULT_DIRS: &[&str] = &[
    "/lib/aarch64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/lib",
    "/usr/lib",
];

/// Resolve a program's closure.
///
/// `read` maps a guest path to file contents, so this walks the guest's
/// namespace -- the sysroot -- and never the host's.
///
/// `place` is asked where to put each dependency, given how much space it
/// needs. The executable keeps whatever base it was loaded with. Handing the
/// decision out rather than making it here is what lets a build and a run
/// agree on the layout: both call this, and the caller supplies the same
/// policy.
pub fn resolve(
    exe: Image,
    exe_path: &str,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
    place: &mut dyn FnMut(u64) -> u64,
) -> Result<Closure, LoadError> {
    let mut out = Closure::default();
    let mut seen: HashSet<String> = HashSet::new();
    // Name -> (asked by, depth), in the order they were first named. A
    // `BTreeMap` would sort them; the order `ld.so` resolves in is the order
    // they were named, so this is a queue.
    let mut queue: Vec<(String, String, u32)> = Vec::new();

    for n in &exe.needed {
        queue.push((n.clone(), exe_path.to_string(), 1));
    }
    let exe_runpath = expand_origin(&exe.runpath, exe_path);
    // Every object's runpath is in scope for what *it* names, and the
    // executable's is in scope for everything: that is `DT_RUNPATH`'s scoping
    // rule, and getting it wrong means finding the wrong libstdc++.
    let mut runpath_of: BTreeMap<String, Vec<String>> = BTreeMap::new();

    seen.insert(exe_path.to_string());
    if let Some(s) = &exe.soname {
        seen.insert(s.clone());
    }
    let exe_span = exe.brk.saturating_sub(exe.bias);
    out.objects.push(Object {
        path: exe_path.to_string(),
        name: basename(exe_path).to_string(),
        image: exe,
        span: exe_span,
        depth: 0,
    });

    let mut i = 0;
    while i < queue.len() {
        let (name, asker, depth) = queue[i].clone();
        i += 1;
        if !seen.insert(name.clone()) {
            continue;
        }
        let mut dirs: Vec<String> = runpath_of.get(&asker).cloned().unwrap_or_default();
        dirs.extend(exe_runpath.iter().cloned());
        dirs.extend(DEFAULT_DIRS.iter().map(|d| d.to_string()));
        let Some((path, bytes)) = find(&name, &dirs, read) else {
            out.missing.push((name, asker));
            continue;
        };
        // Sized before it is placed: an object's span is not known until it
        // is loaded, and it has to be loaded somewhere to be measured. Load
        // it at zero to measure, then again where it belongs. The first load
        // is cheap -- it copies segments into a buffer -- and it is the only
        // way to ask the placer for the right amount.
        let probe = load(&bytes, 0)?;
        let span = probe.brk;
        let base = place(span);
        let image = load(&bytes, base)?;
        for n in &image.needed {
            queue.push((n.clone(), path.clone(), depth + 1));
        }
        if !image.runpath.is_empty() {
            runpath_of.insert(path.clone(), expand_origin(&image.runpath, &path));
        }
        out.objects.push(Object {
            path,
            name: image.soname.clone().unwrap_or_else(|| name.clone()),
            image,
            span,
            depth,
        });
    }
    Ok(out)
}

/// `$ORIGIN` is the directory of the object that named the path.
fn expand_origin(paths: &[String], of: &str) -> Vec<String> {
    let dir = of.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
    paths
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.replace("$ORIGIN", dir).replace("${ORIGIN}", dir))
        .collect()
}

fn basename(path: &str) -> &str {
    path.rsplit_once('/').map(|(_, b)| b).unwrap_or(path)
}

/// The first directory in which `name` exists, and its contents.
///
/// A name containing a slash is a path and is not searched for, which is what
/// `ld.so` does and what makes `$ORIGIN`-relative names work.
fn find(
    name: &str,
    dirs: &[String],
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Option<(String, Vec<u8>)> {
    if name.contains('/') {
        return read(name).map(|b| (name.to_string(), b));
    }
    for d in dirs {
        let p = format!("{}/{}", d.trim_end_matches('/'), name);
        if let Some(b) = read(&p) {
            return Some((p, b));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_expands_to_the_naming_objects_directory() {
        let got = expand_origin(
            &["$ORIGIN/../lib".to_string(), "/opt/lib".to_string()],
            "/usr/bin/thing",
        );
        assert_eq!(got, vec!["/usr/bin/../lib", "/opt/lib"]);
    }

    #[test]
    fn a_name_with_a_slash_is_taken_as_a_path() {
        // The dirs are deliberately somewhere else: a name with a slash must
        // not be joined to them, so this can only succeed by being read as
        // the path it is.
        let read = |p: &str| (p == "sub/libfoo.so").then(|| vec![1u8]);
        assert!(find("sub/libfoo.so", &["/elsewhere".to_string()], &read).is_some());
    }

    #[test]
    fn a_bare_name_is_searched_for_in_order() {
        let read = |p: &str| (p == "/second/libfoo.so").then(|| vec![1u8]);
        let dirs = ["/first".to_string(), "/second".to_string()];
        assert_eq!(
            find("libfoo.so", &dirs, &read).map(|(p, _)| p),
            Some("/second/libfoo.so".to_string()),
        );
    }
}
