//! `gexc eject run <project>` (the project's `run.sh`): run the built program
//! natively, from the project alone.

use crate::build::{inputs_id, rootfs, BuildManifest};
use crate::config::Config;
use crate::project::{rootfs_rel, Project};
use anyhow::{anyhow, bail, Context, Result};
use gex_native::aot::{Aot, Replacement};
use gex_native::{Exit, RunOptions};
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct RunFlags {
    /// Arguments after `argv[0]`; `None` for the configured or recorded ones.
    pub args: Option<Vec<String>>,
    /// Translate code the build never saw instead of failing on it.
    pub jit_fallback: bool,
    pub trace: bool,
    pub verbose: bool,
}

/// The runtime's view of a built project.
pub fn aot(dir: &Path, manifest: &BuildManifest) -> Aot {
    let mut aot = Aot::new(format!("the ejected project {}", dir.display()));
    for i in &manifest.images {
        aot.images.insert(i.identity.clone(), dir.join(&i.module));
    }
    let mut modules: Vec<String> = Vec::new();
    for r in &manifest.replacements {
        let module = match modules.iter().position(|m| *m == r.module) {
            Some(i) => i,
            None => {
                modules.push(r.module.clone());
                modules.len() - 1
            }
        };
        // Ids are dense and in order: `build` numbers them by position.
        debug_assert_eq!(aot.replacements.len(), r.id as usize);
        aot.replacements.push(Replacement {
            module,
            export: r.export.clone(),
            abi: r.abi,
            symbol: r.symbol.clone(),
            addr: r.addr,
            calls: Default::default(),
        });
    }
    aot.modules = modules.iter().map(|m| dir.join(m)).collect();
    aot
}

pub fn run(dir: &Path, flags: &RunFlags) -> Result<Exit> {
    let project = Project::load(dir)?;
    if project.isa != gex_abi::ARCH_NAME {
        bail!("the project is for {} guests and this gexc runs {} guests", project.isa, gex_abi::ARCH_NAME);
    }
    let manifest = BuildManifest::load(dir)?;
    let modules: Vec<String> = manifest.replacements.iter().map(|r| r.module.clone()).collect();
    if inputs_id(dir, &modules)? != manifest.inputs {
        bail!(
            "{} changed since it was built (gex-project.json, user/eject.json or a replacement \
             module); run ./build.sh",
            dir.display()
        );
    }
    let (config, _) = Config::load(dir)?;
    let root = rootfs(dir);
    let root = std::fs::canonicalize(&root).with_context(|| format!("{} (run ./build.sh)", root.display()))?;

    let exe = &project.program.exe;
    let host_exe = root.join(rootfs_rel(exe)?.strip_prefix("rootfs")?);
    let bytes = std::fs::read(&host_exe).with_context(|| format!("reading {}", host_exe.display()))?;
    let rootc = root.clone();
    let program = gex_ld::load_program(&bytes, gex_abi::PIE_BASE, gex_abi::INTERP_BASE, &|guest| {
        std::fs::read(rootc.join(guest.trim_start_matches('/'))).ok()
    })
    .map_err(|e| anyhow!("loading {exe}: {e:?}"))?;

    let argv0 = project
        .program
        .argv
        .first()
        .cloned()
        .unwrap_or_else(|| exe.clone());
    let rest = match (&flags.args, &config.args) {
        (Some(a), _) => a.clone(),
        (None, Some(a)) => a.clone(),
        (None, None) => project.program.argv.iter().skip(1).cloned().collect(),
    };
    let mut args = vec![argv0];
    args.extend(rest);
    let env = if project.program.env.is_empty() {
        vec!["PATH=/usr/bin:/bin".into(), "HOME=/root".into()]
    } else {
        project.program.env.clone()
    };
    let aot = Arc::new(aot(dir, &manifest));
    let opts = RunOptions {
        args,
        env,
        trace: flags.trace,
        verbose: flags.verbose,
        sysroot: Some(root),
        cwd: project.program.cwd.clone(),
        jit: flags.jit_fallback,
        aot: Some(Arc::clone(&aot)),
        ..RunOptions::default()
    };
    let (exit, _) = gex_native::run_program(&program, &opts, &gex_xlate::Options::default())?;
    if flags.verbose {
        for r in &aot.replacements {
            eprintln!(
                "[gex-eject] {} at {:#x} -> {}: {} calls",
                r.symbol,
                r.addr,
                r.export,
                r.calls.load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }
    Ok(exit)
}
