//! Which parts of gex's runtime an ejected build keeps (`user/eject.json`
//! `"features"`, docs/eject.md "Features").
//!
//! An ejected project starts as the runtime a gex page runs, everything in;
//! each feature switched off is taken out of the build. A feature is only
//! listed here once something can mean it, and switching one off that no
//! build can yet remove is an error naming why, never a silent no-op: a
//! setting that does nothing would read as a smaller build that is not.

use anyhow::{bail, Result};

/// One part of the runtime a build can keep or leave out.
pub struct Feature {
    pub name: &'static str,
    /// What it is, for the template and the errors.
    pub about: &'static str,
    /// Why a build cannot leave it out yet, or `None` if it can (for the
    /// kinds of project [`check`] is given).
    pub fixed: Option<&'static str>,
}

/// Every feature, in the order the template lists them.
pub const FEATURES: &[Feature] = &[
    Feature {
        name: "compositor",
        about: "the scene compositor: every window drawn into one WebGPU canvas in a worker, \
                with server-side decorations and its own cursor and hit testing. Off: each \
                window is its own canvas in the page, laid out by the browser (a program that \
                draws its own frames, as Wine does, needs nothing more)",
        fixed: None,
    },
    Feature {
        name: "gl",
        about: "the guest's OpenGL: Mesa on WebGPU (gex-wgpu-replay), in the page. Off: the \
                page ships no Mesa",
        fixed: None,
    },
    Feature {
        name: "multiprocess",
        about: "more than one guest process (fork, exec, a process per Worker)",
        fixed: Some("a W3 program is Wine, whose wineserver, services and the program itself \
                     are separate processes"),
    },
    Feature {
        name: "translation",
        about: "run-time translation (tier-1 and the function JIT) behind the build's units, \
                for code the recording never reached. Off, a wine32 page's default: the page \
                translates nothing, and code no unit holds stops the run, naming it. Off by \
                switch for now (the page's aot=1): the translator is still in the page. \
                \"cold\": \"jit\" keeps the function JIT alone, and wants this left unset",
        fixed: None,
    },
];

/// The feature named `name`.
pub fn feature(name: &str) -> Option<&'static Feature> {
    FEATURES.iter().find(|f| f.name == name)
}

/// Check a set of features switched off: every name known, none that cannot
/// be removed yet.
pub fn check(off: &[String]) -> Result<()> {
    for name in off {
        let Some(f) = feature(name) else {
            let known: Vec<&str> = FEATURES.iter().map(|f| f.name).collect();
            bail!("user/eject.json: unknown feature \"{name}\" (known: {})", known.join(", "));
        };
        if let Some(why) = f.fixed {
            bail!("user/eject.json: \"{name}\" cannot be switched off yet: {why}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_names_and_what_cannot_go() {
        assert!(check(&[]).is_ok());
        assert!(check(&["compositor".into(), "gl".into()]).is_ok());
        assert!(check(&["compsitor".into()]).unwrap_err().to_string().contains("unknown feature"));
        assert!(check(&["multiprocess".into()]).unwrap_err().to_string().contains("cannot be switched off yet"));
        assert!(check(&["translation".into()]).is_ok());
    }
}
