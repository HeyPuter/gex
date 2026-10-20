//! Which objects a program actually consists of.
//!
//! Reads `DT_NEEDED` transitively — the walk `ld.so` does, but here without
//! running anything, so the whole closure (not just the wrapper executable)
//! can be recompiled ahead of time.
//!
//! Search order is `ld.so`'s: `DT_RUNPATH`, then the multiarch directories,
//! then `/lib` and `/usr/lib`. The `ld.so.cache` and hardware-capability
//! subdirectories are skipped, as an unpacked sysroot has nothing in them.

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

/// The directories `ld.so` looks in, after any `DT_RUNPATH`: Debian's
/// multiarch directories for the guest architecture, then the classic pair.
pub const DEFAULT_DIRS: &[&str] = if gex_abi::X86 {
    &[
        "/lib/x86_64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/lib64",
        "/usr/lib64",
        "/lib",
        "/usr/lib",
    ]
} else {
    &[
        "/lib/aarch64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/lib",
        "/usr/lib",
    ]
};

/// Resolve a program's closure.
///
/// `read` maps a guest path to file contents (the sysroot, never the host).
/// `place` is asked where each dependency goes; handing that decision to the
/// caller is what lets a build and a run agree on the layout.
pub fn resolve(
    exe: Image,
    exe_path: &str,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
    place: &mut dyn FnMut(&str, u64) -> u64,
) -> Result<Closure, LoadError> {
    resolve_all(vec![(exe, exe_path.to_string())], read, place)
}

/// Resolve the closure of several programs at once, sharing one layout.
///
/// A program is not only its executable: apt also forks helpers that pull in
/// libraries `apt-get` never touches. Every root contributes its `DT_NEEDED`
/// graph to one set, deduplicated by resolved path and placed once by the
/// shared `place`. The roots come first, in order, so `objects[0]` is still
/// the program.
pub fn resolve_all(
    roots: Vec<(Image, String)>,
    read: &dyn Fn(&str) -> Option<Vec<u8>>,
    place: &mut dyn FnMut(&str, u64) -> u64,
) -> Result<Closure, LoadError> {
    let mut out = Closure::default();
    let mut seen: HashSet<String> = HashSet::new();
    // Name -> (asked by, depth), in the order they were first named. A
    // `BTreeMap` would sort them; the order `ld.so` resolves in is the order
    // they were named, so this is a queue.
    let mut queue: Vec<(String, String, u32)> = Vec::new();
    let mut exe_runpath: Vec<String> = Vec::new();
    // Every object's runpath is in scope for what *it* names, and the
    // executable's is in scope for everything: that is `DT_RUNPATH`'s scoping
    // rule, and getting it wrong means finding the wrong libstdc++.
    let mut runpath_of: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for (n_root, (exe, exe_path)) in roots.into_iter().enumerate() {
        for n in &exe.needed {
            queue.push((n.clone(), exe_path.clone(), 1));
        }
        exe_runpath.extend(expand_origin(&exe.runpath, &exe_path));
        let fresh = seen.insert(exe_path.clone());
        if let Some(s) = &exe.soname {
            seen.insert(s.clone());
        }
        // Every root becomes an object, not just the first: a root left out of
        // the closure is translated from scratch on every run.
        if !fresh {
            continue;
        }
        let _ = n_root;
        let exe_span = exe.brk.saturating_sub(exe.bias);
        out.objects.push(Object {
            path: exe_path.clone(),
            name: basename(&exe_path).to_string(),
            image: exe,
            span: exe_span,
            depth: 0,
        });
    }

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
        let base = place(&path, span);
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

    /// Every shared library is linked at zero, and `resolve` measures one by
    /// loading it there before asking where it goes. That load must not be
    /// refused as "in the runtime's region": nothing is placed at zero.
    #[test]
    fn a_library_linked_at_zero_is_measured_and_placed() {
        let mut exe = crate::elf::load(&crate::elf::test_image(2, gex_abi::EXEC_BASE), 0).unwrap();
        exe.needed = vec!["libzero.so".to_string()];
        let so = crate::elf::test_image(3, 0);
        let read = |p: &str| (p == "/lib/libzero.so").then(|| so.clone());
        let mut asked = Vec::new();
        let mut place = |path: &str, span: u64| {
            asked.push((path.to_string(), span));
            gex_abi::MMAP_BOTTOM
        };
        let c = resolve(exe, "/bin/prog", &read, &mut place).unwrap();
        assert!(c.missing.is_empty(), "{:?}", c.missing);
        assert_eq!(asked, vec![("/lib/libzero.so".to_string(), 0x1000)]);
        assert_eq!(c.objects[1].image.bias, gex_abi::MMAP_BOTTOM);
    }

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
