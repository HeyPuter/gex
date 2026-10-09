//! gex-dylink: gexcc's build-time tool for wasm32 shared libraries.
//!
//!   gex-dylink selfbind IN OUT          bind a library's imports of its own
//!                                       functions (gex_dylink::selfbind)
//!   gex-dylink section IN NAME DATA OUT add custom section NAME (DATA: a file)
//!   gex-dylink fpcast-prep IN OUT       before binaryen --fpcast-emu: every
//!   gex-dylink fpcast-post IN OUT       export gets a thunk, exported after
//!   gex-dylink info FILE                print dylink.0, NEEDED and the table plan
use gex_dylink::*;

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let read = |p: &str| std::fs::read(p).with_context(|| format!("reading {p}"));
    match a.get(1).map(String::as_str) {
        Some("selfbind") if a.len() == 4 => {
            let out = selfbind(&read(&a[2])?)?;
            std::fs::write(&a[3], out)?;
        }
        Some("fpcast-prep") if a.len() == 4 => {
            std::fs::write(&a[3], fpcast_prep(&read(&a[2])?)?)?;
        }
        Some("fpcast-post") if a.len() == 4 => {
            std::fs::write(&a[3], fpcast_post(&read(&a[2])?)?)?;
        }
        Some("section") if a.len() == 6 => {
            let out = set_custom_section(&read(&a[2])?, &a[3], &read(&a[4])?)?;
            std::fs::write(&a[5], out)?;
        }
        Some("info") if a.len() == 3 => {
            let i = Info::parse(&read(&a[2])?)?;
            let (len, _) = i.slots();
            println!(
                "dylink: {} mem {} align {} table {} (range {len})",
                i.dylink, i.mem_size, i.mem_align, i.table_size
            );
            println!("needed: {:?}", i.needed);
            println!("runpath: {:?}", i.runpath);
            let env: Vec<&str> = i
                .imports
                .iter()
                .filter(|x| x.module == "env" && !shim_import("env", &x.name))
                .map(|x| x.name.as_str())
                .collect();
            println!("env imports ({}): {}", env.len(), env.join(" "));
            let got: Vec<String> = i
                .imports
                .iter()
                .filter(|x| x.module.starts_with("GOT."))
                .map(|x| {
                    format!(
                        "{}.{}{}",
                        x.module,
                        x.name,
                        if x.weak { "(weak)" } else { "" }
                    )
                })
                .collect();
            println!("GOT ({}): {}", got.len(), got.join(" "));
            if let Some(f) = &i.forkreach {
                println!("forkreach: {} exports may fork", f.may_fork.len());
            }
        }
        _ => bail!("usage: gex-dylink selfbind IN OUT | section IN NAME DATA OUT | info FILE"),
    }
    Ok(())
}
