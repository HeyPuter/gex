//! Stateful providers an eject project builds from its sources
//! (user/eject.json "providers"): what they include and may import.

use anyhow::{bail, Context, Result};
use std::path::Path;

/// What a provider includes (gex-cgen's), for `gen/provider.h`.
pub const PROVIDER_H: &str = include_str!("../../gex-cgen/providers/provider.h");

/// The imports a stateful provider may have.
fn check_provider_imports(wasm: &[u8], what: &str) -> Result<()> {
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let wasmparser::Payload::ImportSection(r) = payload? {
            for i in r {
                let i = i?;
                let ok = match i.module {
                    "env" => matches!(i.name, "memory" | "__indirect_function_table" | "__stack_pointer" | "__memory_base" | "__table_base"),
                    "gex" => i.name.starts_with("provider_"),
                    _ => false,
                };
                if !ok {
                    bail!("{what} imports {}.{}, which a provider cannot have (give it a definition)", i.module, i.name);
                }
            }
        }
    }
    Ok(())
}

/// Build a stateful provider from its sources (`dir/build.sh OUT GEN`, GEN
/// holding provider.h and gex_abi.h) into `out`, and check what it imports.
pub(crate) fn build_stateful(dir: &Path, p: &crate::config::Provider, out: &Path, gen: &Path) -> Result<Vec<u8>> {
    std::fs::create_dir_all(out.parent().unwrap())?;
    let script = dir.join(&p.dir).join("build.sh");
    let status = std::process::Command::new("sh")
        .arg(&script)
        .arg(out)
        .arg(gen)
        .status()
        .with_context(|| format!("running {}", script.display()))?;
    if !status.success() {
        bail!("building the provider for {} ({}) failed", p.library, script.display());
    }
    let wasm = std::fs::read(out)?;
    check_provider_imports(&wasm, &format!("the provider for {}", p.library))?;
    Ok(wasm)
}

